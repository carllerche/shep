//! Safe, local io_uring building blocks for Shep.
//!
//! [`Io::push`] owns and publishes an [`Operation`] without submitting it.
//! [`Driver::submit`] submits a batch; [`Driver::reap`] only processes CQEs.
//! [`OperationHandle::poll_complete`] observes an operation's shared lifecycle.
//! Handles contain only a packed ID. Pass the allocating Io to complete or
//! explicitly release them; adapters release dropped futures with borrowed guards.
//! Cancel operations also retain their target identity until their CQE retires.
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp_io2::Io>();
//! ```
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp_io2::Driver>();
//! ```
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
mod buffer;
mod driver;
mod lifecycle;
mod operation;
pub use buffer::Buffer;
pub use driver::{
    CancelTarget, Completion, Driver, Io, OperationHandle, Progress, Rejected, Value, WaitOutcome,
};
pub use operation::{Close, Operation, SocketAddress};
