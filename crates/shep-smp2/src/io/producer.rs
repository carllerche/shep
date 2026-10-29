//! Safe local publication and admission interface for a driver's shared state.
#![forbid(unsafe_code)]
use crate::io::{Operation, OperationHandle, driver::Shared};
use std::{io, rc::Rc};

/// Admission or publication failed; the caller still owns every unsubmitted input.
#[derive(Debug)]
pub struct Rejected {
    /// Validation, closed-driver, submission, or bounded-progress error; no waiter is queued.
    pub error: io::Error,
    /// Original owned request, including its reusable buffer when applicable.
    pub operation: Operation,
}
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for Rejected {}

/// Local I/O admission and publication handle.
/// Publication turns over a full SQ without waiting for I/O or queuing rejected work.
#[derive(Clone)]
pub struct Io {
    /// Shared local ownership of the ring and slab; ordinary operation handles do not clone it.
    pub(crate) shared: Rc<Shared>,
}
impl Io {
    /// Publish owned inputs as a new Submitted record and return its submitter claim.
    /// If the SQ is full, make at most two nonblocking submit/reap attempts and
    /// retry. Exhausted progress or reentrant borrows return WouldBlock; other
    /// validation/admission/submission errors also return the unpublished inputs.
    /// Already-published requests survive partial submissions and enter errors.
    /// No software backlog or outstanding-operation cap is imposed.
    /// Reaped guards, retired inputs and wakers run outside driver borrows before
    /// the new request is published. No callbacks run after publication.
    /// CLOSE transfers its descriptor only after publication.
    pub fn push(&self, operation: Operation) -> Result<OperationHandle, Rejected> {
        self.shared.push(operation)
    }
    /// Publish a request with adapter resources retained until its terminal CQE.
    /// The guard is dropped outside driver borrows during tick, even if the
    /// submitter releases its handle. On rejection it is dropped immediately.
    /// Guards must not panic in Drop. This does not retain resources until the
    /// result is consumed, nor until cancellation identity pins are released.
    pub fn push_with_guard<G: 'static>(
        &self,
        operation: Operation,
        guard: G,
    ) -> Result<OperationHandle, Rejected> {
        let handle = self.push(operation)?;
        // push performs all pressure callbacks before publication. Nothing can
        // reap this operation between its publication and guard installation.
        self.shared
            .state
            .borrow_mut()
            .arena
            .record(handle.id.unwrap())
            .completion_guard = Some(Box::new(guard));
        Ok(handle)
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
