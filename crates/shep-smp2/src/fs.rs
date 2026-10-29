//! Core-local asynchronous filesystem operations over the I/O driver.
//!
//! Transfers own their buffers and expose short results. A full submission queue
//! triggers bounded progress; stalled publication returns WouldBlock. Namespace and metadata helpers use a bounded worker, never the
//! core thread. Callers order writes, syncs and directory syncs explicitly.
//! Higher layers coordinate admission and shutdown. Each operation retains its
//! kernel resources, including when its future is abandoned.
#![forbid(unsafe_code)]
#![doc = include_str!("../docs/fs/README.md")]

mod file;
mod filesystem;
mod open_options;

pub use crate::io::Buffer;
pub use file::File;
pub use filesystem::{create_dir_all, read_dir, remove_file, rename, sync_dir};
pub use open_options::OpenOptions;

#[cfg(all(test, not(miri)))]
mod tests;
