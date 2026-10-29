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

impl Lifecycle {
    /// Original kernel ownership ended; only a result or identity pins remain.
    pub(crate) fn is_complete(&self) -> bool {
        matches!(self, Self::Completed(_) | Self::VacantPinned)
    }
}
