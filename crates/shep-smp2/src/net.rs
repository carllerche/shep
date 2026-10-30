//! Local TCP with owned buffers, exclusive directions, and no write backlog.
//!
//! Futures are lazy. Stalled ordinary SQ admission returns `WouldBlock`.
//! Dropping an admitted read/write/accept requests cancellation without closing
//! the socket. Named operation futures also offer `cancel().await`, which waits
//! for the original terminal result. Publication is best effort and failed
//! cancellation may leave it waiting indefinitely. Cancellation may race I/O and
//! does not undo bytes transferred. Dropping the socket itself remains terminal.
//! `flush` checks connection state; it does not wait for abandoned operations
//! or imply peer acknowledgement.
#![forbid(unsafe_code)]
#![doc = include_str!("../docs/net/README.md")]

mod config;
mod future;
mod read_half;
mod socket;
mod tcp_listener;
mod tcp_stream;
mod write_half;

pub use crate::io::Buffer;
pub use config::Config;
pub use future::{Accept, Read, Write, WriteAll};
pub use read_half::ReadHalf;
pub use tcp_listener::TcpListener;
pub use tcp_stream::TcpStream;
pub use write_half::WriteHalf;
