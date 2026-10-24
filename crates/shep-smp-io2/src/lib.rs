//! Local owned-buffer file futures and bounded admission, independent of Tokio.
//! The runtime drives [`Reactor`]; applications clone [`Io`] and [`File`].
//! Dropping an accepted operation's future detaches its result, preserving all
//! kernel-visible resources and admission capacity through actual completion.
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp_io2::Io>();
//! ```
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp_io2::Reactor>();
//! ```
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<shep_smp_io2::Reactor>();
//! ```
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

mod core;
mod io;

pub use core::{Buffer, WaitOutcome, WakeHandle};
pub use io::{File, Io, Park, Reactor};
