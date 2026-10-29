//! Safe local publication and admission interface for a driver's shared state.
#![forbid(unsafe_code)]
use crate::io::{Op, Operation, OperationData, driver::Shared};
use std::{io, rc::Rc};

/// Admission or publication failed; the caller still owns every unsubmitted input.
#[derive(Debug)]
pub struct Rejected<T = Operation> {
    /// Validation, closed-driver, submission, or bounded-progress error; no waiter is queued.
    pub error: io::Error,
    /// Original owned request, including its reusable buffer when applicable.
    pub operation: T,
}
impl<T> std::fmt::Display for Rejected<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl<T: std::fmt::Debug> std::error::Error for Rejected<T> {}

/// Local I/O admission and publication handle.
/// Publication turns over a full SQ without waiting for I/O or queuing rejected work.
#[derive(Clone)]
pub struct Io {
    /// Shared local ownership of the ring and slab; operation owners retain it.
    pub(crate) shared: Rc<Shared>,
}
impl Io {
    /// Publish owned inputs as a new Submitted record and return its submitter claim.
    /// If the SQ is full, make at most two nonblocking submit/reap attempts and
    /// retry. Exhausted progress or reentrant borrows return WouldBlock; other
    /// validation/admission/submission errors also return the unpublished inputs.
    /// Already-published requests survive partial submissions and enter errors.
    /// No software backlog or outstanding-operation cap is imposed.
    /// Retired inputs and wakers run outside driver borrows before
    /// the new request is published. No callbacks run after publication.
    /// CLOSE transfers its descriptor only after publication.
    pub fn push(&self, operation: Operation) -> Result<Op, Rejected> {
        self.submit(operation)
    }
    /// Publish built-in concrete inputs, returning them unchanged on rejection.
    /// Normal submission and completion do not allocate an operation box.
    pub fn submit<T: OperationData>(&self, mut operation: T) -> Result<Op<T>, Rejected<T>> {
        match self.shared.push(&mut operation) {
            Ok(id) => Ok(Op::new(self.clone(), id, operation)),
            Err(error) => Err(Rejected { error, operation }),
        }
    }
    /// Count accepted SQEs whose terminal CQEs have not been reaped; pins and
    /// retained completed results do not count. Includes the runtime's watch, if any.
    pub fn in_flight(&self) -> usize {
        self.shared.state.borrow().arena.in_flight()
    }
    /// Whether published SQEs still await submission; false once the ring is gone.
    pub fn has_pending(&self) -> bool {
        self.shared.has_pending()
    }
    /// Close ordinary admission; cancellation remains possible via SQ turnover.
    /// Existing record states and kernel ownership are unchanged.
    pub fn close(&self) {
        self.shared.state.borrow_mut().arena.close();
        self.shared.worker.close();
    }
    pub fn is_failed(&self) -> bool {
        self.shared.state.borrow().arena.is_failed()
    }
}
