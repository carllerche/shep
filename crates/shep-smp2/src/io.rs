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
//! [`Io::push`] owns and publishes an [`Operation`] without submitting it.
//! [`Driver::submit`] submits a batch; [`Driver::reap_into`] only processes CQEs.
//! [`OperationHandle::poll_complete`] observes an operation's shared lifecycle.
//! Handles contain only a packed ID. Pass the allocating Io to complete or
//! explicitly release them; adapters release dropped futures with borrowed guards.
//! Cancel operations also retain their target identity until their CQE retires.
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
pub use buffer::Buffer;
pub use driver::{Driver, Progress, WaitOutcome};
pub use fd::Fd;
pub use operation::{
    CancelTarget, Close, Completion, Operation, OperationHandle, SocketAddress, Value,
};
pub use producer::{Io, Rejected};
