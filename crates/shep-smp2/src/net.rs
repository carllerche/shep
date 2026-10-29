//! Local TCP with owned buffers, exclusive directions, and no write backlog.
//!
//! Futures are lazy. Stalled SQ progress returns `WouldBlock`. Dropping an
//! admitted accept/read/write/connect future shuts down the entire resource:
//! cancellation is terminal, and may leave partial network effects. Unpolled
//! futures have no effects. Runtime shutdown can therefore retire abandoned
//! socket operations even when cancellation publication cannot make progress.
//! `flush` means all preceding writes completed locally, not peer acknowledgement.
#![forbid(unsafe_code)]
#![doc = include_str!("../docs/net/README.md")]

mod config;
mod read_half;
mod socket;
mod tcp_listener;
mod tcp_stream;
mod write_half;

pub use crate::io::Buffer;
pub use config::Config;
pub use read_half::ReadHalf;
pub use tcp_listener::TcpListener;
pub use tcp_stream::TcpStream;
pub use write_half::WriteHalf;
