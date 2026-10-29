//! Safe, local io_uring building blocks for Shep.
//!
//! Requires Linux 6.9 or newer. Positional writes always use `RWF_NOAPPEND`;
//! unsupported flags or operations fail through their completions, without a
//! fallback that weakens positional semantics. [`Driver::new`] does not check
//! the kernel version or probe support for every operation.
//!
//! [`Fd`] owns a descriptor validated for io2. Generic imports validate once;
//! CLOSE admission performs no descriptor validation syscall. Standard File and
//! socket wrappers use checked conversions because they can contain arbitrary fds.
//!
//! [`Io::push`] owns and publishes an [`Operation`], submitting existing SQEs
//! and reaping available completions when the bounded submission queue is full.
//! Tracking grows independently of SQ capacity; no software pending queue is used.
//! [`Driver::submit`] submits a batch; [`Driver::tick`] retires CQEs and delivers notifications.
//! [`Op::poll_complete`] observes an operation's shared lifecycle.
//! Typed [`Op`] owners retain inputs and can be awaited directly. Drop abandons
//! a result, boxing inputs only while the kernel still needs them.
//! Cancel operations also retain their target identity until their CQE retires.
//!
//! Before destroying the driver, callers should stop submission and retire every
//! accepted operation, explicitly canceling indefinite operations as needed.
//! [`Driver::drain`] waits for terminal completions. Destruction makes a
//! best-effort cancel-and-drain attempt for remaining work and aborts the process
//! on failure to establish safe retirement. A cancellation request or closing
//! the ring alone does not establish that kernel access to inputs has ended.
//! Completed results and identity-only cancellation pins may outlive the driver.
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp2::io::Io>();
//! ```
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp2::io::Driver>();
//! ```
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
mod arena;
mod buffer;
mod driver;
mod fd;
mod lifecycle;
mod operation;
mod producer;
pub(crate) mod support;
pub use buffer::Buffer;
pub use driver::{Driver, WaitOutcome};
pub use fd::Fd;
pub use operation::{
    Accept, CancelTarget, Close, Completion, Connect, Fallocate, Fsync, Ftruncate, Op, OpenAt,
    Operation, OperationData, PollAdd, ReadAt, Recv, Send, SendRange, Shutdown, Socket,
    SocketAddress, Value, WriteAt,
};
pub use producer::{Io, Rejected};
