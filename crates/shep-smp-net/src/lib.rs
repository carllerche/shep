//! Local TCP with owned buffers, exclusive directions, and no write backlog.
//!
//! Futures are lazy. Admission saturation returns `WouldBlock`. Dropping an
//! admitted accept/read/write/connect future shuts down the entire resource:
//! cancellation is terminal, and may leave partial network effects. Unpolled
//! futures have no effects. Runtime shutdown can therefore retire abandoned
//! socket operations even when the driver's cancellation reserve is occupied.
//! `flush` means all preceding writes completed locally, not peer acknowledgement.
#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

mod config;
mod read_half;
mod socket;
mod tcp_listener;
mod tcp_stream;
mod write_half;

pub use config::Config;
pub use read_half::ReadHalf;
pub use shep_smp_io2::Buffer;
pub use tcp_listener::TcpListener;
pub use tcp_stream::TcpStream;
pub use write_half::WriteHalf;
