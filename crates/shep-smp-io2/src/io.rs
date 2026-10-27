//! Safe local publication and admission interface for a driver's shared state.
#![forbid(unsafe_code)]
use crate::{Operation, OperationHandle, driver::Shared};
use std::{io, rc::Rc};

/// Admission or publication failed; the caller still owns every unsubmitted input.
#[derive(Debug)]
pub struct Rejected {
    /// Validation, closed-driver, or immediate-capacity error; no waiter is queued.
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

/// Local push-only producer. It never submits, waits, or queues rejected work.
#[derive(Clone)]
pub struct Io {
    /// Shared local ownership of the ring and slab; ordinary operation handles do not clone it.
    pub(crate) shared: Rc<Shared>,
}
impl Io {
    /// Publish owned inputs as a new Submitted record and return its submitter claim.
    /// No submit syscall, reap, or waiting occurs. Validation/capacity errors return
    /// the operation; failed SQ push rolls Submitted back to removed. CLOSE transfers
    /// its unique File into a kernel-owned fd only after successful publication.
    pub fn push(&self, operation: Operation) -> Result<OperationHandle, Rejected> {
        self.shared.push(operation)
    }
    /// Publish a request with adapter resources retained until its terminal CQE.
    /// The guard is dropped outside driver borrows by Progress::wake after
    /// reaping (or when that Progress is dropped), even if the
    /// submitter releases its handle. On rejection it is dropped immediately.
    /// Guards must not panic in Drop. This does not retain resources until the
    /// result is consumed, nor until cancellation identity pins are released.
    pub fn push_with_guard<G: 'static>(
        &self,
        operation: Operation,
        guard: G,
    ) -> Result<OperationHandle, Rejected> {
        let handle = self.push(operation)?;
        self.shared
            .arena
            .borrow_mut()
            .record(handle.id.unwrap())
            .completion_guard = Some(Box::new(guard));
        Ok(handle)
    }
    /// Count accepted SQEs whose terminal CQEs have not been reaped; pins and
    /// retained completed results do not count. Includes the runtime's watch, if any.
    pub fn in_flight(&self) -> usize {
        self.shared.arena.borrow().in_flight()
    }
    /// Whether published SQEs still await submission; false once the ring is gone.
    pub fn has_pending(&self) -> bool {
        self.shared.has_pending()
    }
    /// Open admission -> ordinary-closed; cancellation remains possible through
    /// its reserve. Existing record states and kernel ownership are unchanged.
    pub fn close(&self) {
        self.shared.arena.borrow_mut().close();
    }
    pub fn is_failed(&self) -> bool {
        self.shared.arena.borrow().is_failed()
    }
}
