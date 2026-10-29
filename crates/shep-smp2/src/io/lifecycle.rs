//! Submitter interest, completion storage, and abandoned kernel inputs.
#![forbid(unsafe_code)]
use crate::io::Value;
use std::{any::Any, io, task::Waker};
pub(crate) enum Lifecycle {
    Submitted,
    Waiting(Waker),
    Ignored(Option<Box<dyn Any>>),
    Completed(io::Result<Value>),
    /// Only cancellation identity pins remain. No input/result resources remain.
    VacantPinned,
}
