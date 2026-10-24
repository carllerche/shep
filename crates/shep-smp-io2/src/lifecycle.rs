//! Per-operation submitter interest and completion state.
//! The arena separately tracks kernel references and decides when a slot is reusable.
#![forbid(unsafe_code)]
use crate::Value;
use std::{io, task::Waker};

/// Submitter interest and completion availability, independent of cancellation pins.
/// A slot is reusable only in Ignored with no remaining kernel references.
pub(crate) enum Lifecycle {
    /// Published or awaiting a CQE, with an interested submitter and no waker.
    Submitted,
    /// Still awaiting a CQE; notify this submitter when it is reaped.
    Waiting(Waker),
    /// Submitter released its claim. None means no result, or values consumed;
    /// Some retains an abandoned CQE result until cancellation pins retire.
    Ignored(Option<io::Result<Value>>),
    /// Original CQE reaped; the submitter has not consumed its owned result yet.
    Completed(io::Result<Value>),
}
