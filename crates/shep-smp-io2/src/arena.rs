//! Operation slab ownership, admission, and packed slot identities.
#![forbid(unsafe_code)]
use crate::{Completion, Operation, Rejected, Value, driver::DriverId, lifecycle::Lifecycle};
use slab::Slab;
use std::{io, task::Waker};

const INDEX_BITS: u32 = 40;
const MAX_INDEX: u64 = (1 << INDEX_BITS) - 1;

/// Owner and slab index packed into the u64 carried by SQEs and CQEs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OperationId(
    /// High 24 bits identify the driver; low 40 bits select its slab entry.
    std::num::NonZeroU64,
);
impl OperationId {
    /// Pack this owner and a slab index; return None if the index exceeds 40 bits.
    pub(crate) fn new(owner: DriverId, index: usize) -> Option<OperationId> {
        let index = u64::try_from(index).ok()?;
        if index > MAX_INDEX {
            return None;
        }
        Some(OperationId(
            std::num::NonZeroU64::new((u64::from(owner.raw()) << INDEX_BITS) | index)
                .expect("nonzero I/O identity"),
        ))
    }

    /// Decode kernel user_data; panic on zero. The caller must also check the owner.
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(std::num::NonZeroU64::new(raw).expect("nonzero CQE identity"))
    }
    pub(crate) fn raw(self) -> u64 {
        self.0.get()
    }
    pub(crate) fn belongs_to(self, owner: DriverId) -> bool {
        (self.raw() >> INDEX_BITS) == u64::from(owner.raw())
    }
    /// Panic on an owner mismatch before the caller indexes its slab.
    pub(crate) fn check(self, owner: DriverId) {
        assert!(
            self.belongs_to(owner),
            "operation belongs to a different Io"
        );
    }
    fn index(self) -> usize {
        (self.raw() & MAX_INDEX) as usize
    }
}

/// One shared slab entry, retained until both submitter and kernel claims end.
pub(crate) struct Record {
    /// Owned request inputs. None after consumption while a cancel pins the slot.
    pub(crate) operation: Option<Operation>,
    /// Submitter interest, optional result, and the operation-specific waker.
    lifecycle: Lifecycle,
    /// One reference for the original CQE plus each outstanding CancelTarget pin,
    /// including tokens not yet published as cancellation SQEs.
    kernel_refs: usize,
    /// Whether the original terminal CQE has been reaped; pins may still remain.
    own_done: bool,
    /// Whether this request uses the separate one-operation cancellation reserve.
    cancel: bool,
}
/// Owns requests and enforces admission separately from completed-result retention.
/// Callers validate ID ownership before access and destroy returned records/wakers
/// after releasing arena borrows.
pub(crate) struct Arena {
    /// Owner prefix applied to every newly allocated slab index.
    driver_id: DriverId,
    /// Live records, including completed values and cancellation-pinned identities.
    entries: Slab<Record>,
    /// Accepted operations awaiting their original CQEs, including cancellation SQEs.
    active: usize,
    /// Outstanding cancellation SQEs; at most one, excluded from ordinary capacity.
    cancel_active: usize,
    /// Maximum ordinary operations awaiting CQEs; completed results do not count.
    capacity: usize,
    /// Ordinary admission is closed; the cancellation reserve remains usable.
    closed: bool,
    /// Terminal driver failure; all admission, including cancellation, is rejected.
    failed: bool,
}
impl Arena {
    /// Create an empty arena with ordinary capacity and one reserved cancellation slot.
    pub(crate) fn new(capacity: usize, driver_id: DriverId) -> Self {
        Self {
            driver_id,
            entries: Slab::with_capacity(capacity + 1),
            active: 0,
            cancel_active: 0,
            capacity,
            closed: false,
            failed: false,
        }
    }
    /// Create Submitted with one original-CQE reference and a submitter claim.
    /// BrokenPipe rejects closed ordinary admission or any admission after failure;
    /// WouldBlock rejects exhausted capacity, cancellation reserve, or index space.
    /// On rejection, return inputs unchanged; no lifecycle entry is created.
    pub(crate) fn insert(&mut self, operation: Operation) -> Result<OperationId, Rejected> {
        let cancel = operation.is_cancel();
        let error = if self.failed || (self.closed && !cancel) {
            Some(io::ErrorKind::BrokenPipe)
        } else if (cancel && self.cancel_active != 0)
            || (!cancel && self.active - self.cancel_active == self.capacity)
        {
            // One reserved cancellation permits progress even at the regular limit.
            Some(io::ErrorKind::WouldBlock)
        } else {
            None
        };
        if let Some(error) = error {
            return Err(Rejected {
                operation,
                error: error.into(),
            });
        }
        let entry = self.entries.vacant_entry();
        let Some(id) = OperationId::new(self.driver_id, entry.key()) else {
            return Err(Rejected {
                operation,
                error: io::ErrorKind::WouldBlock.into(),
            });
        };
        entry.insert(Record {
            operation: Some(operation),
            lifecycle: Lifecycle::Submitted,
            kernel_refs: 1,
            own_done: false,
            cancel,
        });
        self.active += 1;
        self.cancel_active += usize::from(cancel);
        Ok(id)
    }
    /// Borrow a live record; panic if its slot is absent. Owner validation is external.
    pub(crate) fn record(&mut self, id: OperationId) -> &mut Record {
        self.entries
            .get_mut(id.index())
            .expect("live operation identity")
    }
    /// Roll back Submitted -> removed after SQ push failed to publish the request.
    /// Requires a fresh, unshared entry: panics unless Submitted with one kernel ref.
    /// Return its owned inputs and release admission; never use after publication.
    pub(crate) fn reject(&mut self, id: OperationId, error: io::Error) -> Rejected {
        let record = self.entries.remove(id.index());
        assert!(matches!(record.lifecycle, Lifecycle::Submitted));
        assert_eq!(record.kernel_refs, 1);
        self.active -= 1;
        self.cancel_active -= usize::from(record.cancel);
        Rejected {
            operation: record.operation.unwrap(),
            error,
        }
    }
    /// Keep a live identity unavailable for reuse by adding a cancellation reference.
    /// Lifecycle is unchanged; panic on a missing slot or reference-count overflow.
    pub(crate) fn pin(&mut self, id: OperationId) {
        let record = self.record(id);
        record.kernel_refs = record
            .kernel_refs
            .checked_add(1)
            .expect("operation reference overflow");
    }
    /// Release one kernel/identity reference. Ignored -> removed only at zero refs;
    /// all other states remain resident. Panic on a missing slot or zero ref count.
    /// Return any removed record for destruction outside the arena borrow.
    pub(crate) fn unpin(&mut self, id: OperationId) -> Option<Record> {
        let record = self.record(id);
        assert!(record.kernel_refs != 0);
        record.kernel_refs -= 1;
        if record.kernel_refs == 0 && matches!(record.lifecycle, Lifecycle::Ignored(_)) {
            Some(self.entries.remove(id.index()))
        } else {
            None
        }
    }
    /// Observe the original terminal CQE: Submitted/Waiting -> Completed(result),
    /// or Ignored(None) -> Ignored(Some(result)); extract any Waiting waker.
    /// Release admission and the original kernel ref, removing Ignored if no pins
    /// remain. Panic on a missing slot, duplicate CQE, or incompatible state.
    /// Return notifications/discards for delivery/destruction outside the borrow.
    pub(crate) fn complete(
        &mut self,
        id: OperationId,
        result: io::Result<Value>,
    ) -> (Option<Waker>, Option<Record>) {
        let record = self.record(id);
        assert!(!record.own_done, "duplicate terminal CQE");
        record.own_done = true;
        let previous = std::mem::replace(&mut record.lifecycle, Lifecycle::Submitted);
        let (wake, next) = match previous {
            Lifecycle::Submitted => (None, Lifecycle::Completed(result)),
            Lifecycle::Waiting(waker) => (Some(waker), Lifecycle::Completed(result)),
            Lifecycle::Ignored(None) => (None, Lifecycle::Ignored(Some(result))),
            _ => unreachable!(),
        };
        record.lifecycle = next;
        self.cancel_active -= usize::from(self.entries[id.index()].cancel);
        self.active -= 1;
        (wake, self.unpin(id))
    }
    /// Consume Completed(result) -> Ignored(None), returning both result and inputs.
    /// Remove the slot if kernel_refs is zero; otherwise keep its now input-free
    /// record until cancellation pins retire. Return None without mutation for an
    /// absent slot or any non-Completed state; never install or invoke a waker.
    pub(crate) fn take(&mut self, id: OperationId) -> Option<Completion> {
        let record = self.entries.get_mut(id.index())?;
        if !matches!(record.lifecycle, Lifecycle::Completed(_)) {
            return None;
        }
        let Lifecycle::Completed(result) =
            std::mem::replace(&mut record.lifecycle, Lifecycle::Ignored(None))
        else {
            unreachable!()
        };
        let operation = record.operation.take().expect("unconsumed operation");
        if record.kernel_refs == 0 {
            self.entries.remove(id.index());
        }
        Some(Completion { result, operation })
    }
    /// Register Submitted -> Waiting(waker), or replace Waiting's waker.
    /// Completed stays Completed and returns the unused new waker; Ignored or a
    /// missing slot panics. Return replaced wakers for destruction outside the borrow.
    pub(crate) fn wait(&mut self, id: OperationId, waker: Waker) -> Option<Waker> {
        match &mut self.record(id).lifecycle {
            state @ Lifecycle::Submitted => {
                *state = Lifecycle::Waiting(waker);
                None
            }
            Lifecycle::Waiting(old) => Some(std::mem::replace(old, waker)),
            Lifecycle::Completed(_) => Some(waker),
            Lifecycle::Ignored(_) => unreachable!("submitter released operation"),
        }
    }
    /// Release the submitter: Submitted/Waiting -> Ignored(None), or Completed ->
    /// Ignored(Some(result)). With no kernel refs, remove the record immediately.
    /// Otherwise retain inputs/results until the final unpin. Panic on a missing
    /// slot or a second release of a retained Ignored record. Return the old waker
    /// and any removed record for destruction outside the borrow.
    pub(crate) fn abandon(&mut self, id: OperationId) -> (Option<Waker>, Option<Record>) {
        let record = self.record(id);
        if record.kernel_refs == 0 {
            return (None, Some(self.entries.remove(id.index())));
        }
        let previous = std::mem::replace(&mut record.lifecycle, Lifecycle::Ignored(None));
        let wake = match previous {
            Lifecycle::Waiting(waker) => Some(waker),
            Lifecycle::Submitted => None,
            Lifecycle::Completed(result) => {
                record.lifecycle = Lifecycle::Ignored(Some(result));
                None
            }
            Lifecycle::Ignored(_) => unreachable!("submitter released twice"),
        };
        (wake, None)
    }
    /// Close ordinary admission without changing record states; cancellation is allowed.
    pub(crate) fn close(&mut self) {
        self.closed = true;
    }
    /// Make admission terminally closed, including cancellation; records stay intact.
    pub(crate) fn fail(&mut self) {
        self.closed = true;
        self.failed = true;
    }
    pub(crate) fn is_failed(&self) -> bool {
        self.failed
    }
    pub(crate) fn in_flight(&self) -> usize {
        self.active
    }
    /// Extract pending wakers with Waiting -> Submitted; leave other states unchanged.
    /// Failure cleanup uses this to notify waiters outside borrows; no SQE is resubmitted.
    pub(crate) fn wakers_into(&mut self, wakes: &mut Vec<Waker>) {
        for (_, record) in &mut self.entries {
            if matches!(record.lifecycle, Lifecycle::Waiting(_)) {
                let Lifecycle::Waiting(waker) =
                    std::mem::replace(&mut record.lifecycle, Lifecycle::Submitted)
                else {
                    unreachable!()
                };
                wakes.push(waker);
            }
        }
    }
    /// Find the next original request to cancel during driver destruction.
    /// Admission must be failed first so no slot can be reused while the cursor
    /// advances. Cancellation requests themselves are reaped without canceling.
    /// The cursor scans allocated storage once, without collecting identities.
    pub(crate) fn next_cancel_target(&self, cursor: &mut usize) -> Option<OperationId> {
        assert!(self.failed);
        while *cursor < self.entries.capacity() {
            let index = *cursor;
            *cursor += 1;
            if let Some(record) = self.entries.get(index)
                && !record.own_done
                && !record.cancel
            {
                return Some(OperationId::new(self.driver_id, index).unwrap());
            }
        }
        None
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Buffer;
    use std::{fs::File, sync::Arc};
    fn operation() -> Operation {
        Operation::ReadAt {
            file: Arc::new(File::open("/dev/null").unwrap()),
            buffer: Buffer::new(8, 8).unwrap(),
            offset: 0,
        }
    }
    #[test]
    fn completion_first_keeps_values_until_the_future_consumes_them() {
        let mut arena = Arena::new(1, DriverId::allocate().unwrap());
        let token = arena.insert(operation()).unwrap();
        assert!(arena.take(token).is_none());
        let (wake, discard) = arena.complete(token, Ok(Value::Count(3)));
        assert!(wake.is_none() && discard.is_none());
        assert_eq!(arena.in_flight(), 0);
        assert_eq!(arena.len(), 1);
        let completion = arena.take(token).unwrap();
        assert!(matches!(completion.result.unwrap(), Value::Count(3)));
        assert_eq!(arena.len(), 0);
        let next = arena.insert(operation()).unwrap();
        assert_eq!(next.index(), token.index());
    }
    #[test]
    fn future_drop_first_retains_resources_until_the_terminal_cqe() {
        let mut arena = Arena::new(1, DriverId::allocate().unwrap());
        let operation = operation();
        let Operation::ReadAt { file, .. } = &operation else {
            unreachable!()
        };
        let weak = Arc::downgrade(file);
        let token = arena.insert(operation).unwrap();
        assert!(arena.abandon(token).1.is_none());
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.in_flight(), 1);
        assert!(weak.upgrade().is_some());
        let (_, discarded) = arena.complete(token, Ok(Value::Count(0)));
        assert_eq!(arena.len(), 0);
        assert_eq!(arena.in_flight(), 0);
        drop(discarded);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn waiting_completion_extracts_waker_before_removing_a_completed_entry() {
        let mut arena = Arena::new(1, DriverId::allocate().unwrap());
        let token = arena.insert(operation()).unwrap();
        assert!(arena.wait(token, Waker::noop().clone()).is_none());
        assert!(arena.wait(token, Waker::noop().clone()).is_some());
        let (wake, discarded) = arena.complete(token, Ok(Value::Count(0)));
        assert!(wake.is_some() && discarded.is_none());
        assert!(arena.take(token).is_some());
        assert_eq!(arena.len(), 0);
        drop(wake); // Driver holds a waker, not an index into a reused slot.
    }
    #[test]
    fn dropping_a_waiting_future_releases_its_waker_but_not_its_operation() {
        let mut arena = Arena::new(1, DriverId::allocate().unwrap());
        let token = arena.insert(operation()).unwrap();
        arena.wait(token, Waker::noop().clone());
        let (wake, discarded) = arena.abandon(token);
        assert!(wake.is_some() && discarded.is_none());
        assert_eq!(arena.in_flight(), 1);
        let (wake, discarded) = arena.complete(token, Ok(Value::Count(0)));
        assert!(wake.is_none() && discarded.is_some());
        assert_eq!(arena.len(), 0);
    }
    #[test]
    fn dropping_a_completed_future_removes_the_entry() {
        let mut arena = Arena::new(1, DriverId::allocate().unwrap());
        let token = arena.insert(operation()).unwrap();
        arena.complete(token, Ok(Value::Count(0)));
        assert!(arena.abandon(token).1.is_some());
        assert_eq!(arena.len(), 0);
    }
    #[test]
    fn completed_results_do_not_consume_kernel_capacity() {
        let mut arena = Arena::new(1, DriverId::allocate().unwrap());
        let first = arena.insert(operation()).unwrap();
        arena.complete(first, Ok(Value::Count(0)));
        let second = arena.insert(operation()).unwrap();
        assert_ne!(first.index(), second.index());
        assert_eq!(arena.len(), 2);
        assert_eq!(arena.in_flight(), 1);
        assert!(arena.take(first).is_some());
        arena.complete(second, Ok(Value::Count(0)));
        assert!(arena.take(second).is_some());
    }
    #[test]
    fn rejection_and_closure_preserve_unaccepted_values() {
        let mut arena = Arena::new(1, DriverId::allocate().unwrap());
        let first = arena.insert(operation()).unwrap();
        assert_eq!(
            arena.insert(operation()).unwrap_err().error.kind(),
            io::ErrorKind::WouldBlock
        );
        let rejected = arena.reject(first, io::ErrorKind::WouldBlock.into());
        assert_eq!(arena.in_flight(), 0);
        arena.close();
        assert_eq!(
            arena.insert(rejected.operation).unwrap_err().error.kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn consumed_completion_keeps_identity_until_cancel_reference_retires() {
        let mut arena = Arena::new(1, DriverId::allocate().unwrap());
        let first = arena.insert(Operation::Nop).unwrap();
        arena.pin(first);
        arena.complete(first, Ok(Value::Done));
        assert!(arena.take(first).is_some());
        assert_eq!(arena.len(), 1);
        let second = arena.insert(Operation::Nop).unwrap();
        assert_ne!(first, second);
        assert!(arena.unpin(first).is_some());
        arena.complete(second, Ok(Value::Done));
        arena.take(second).unwrap();
        assert_eq!(arena.len(), 0);
    }
    #[test]
    fn cancel_reference_retiring_first_preserves_original_kernel_ownership() {
        let mut arena = Arena::new(1, DriverId::allocate().unwrap());
        let id = arena.insert(Operation::Nop).unwrap();
        arena.pin(id);
        arena.abandon(id);
        assert!(arena.unpin(id).is_none());
        assert_eq!(arena.len(), 1);
        assert!(arena.complete(id, Ok(Value::Done)).1.is_some());
        assert_eq!(arena.len(), 0);
    }
    #[test]
    fn ignored_fd_result_is_owned_until_the_final_kernel_reference() {
        let mut arena = Arena::new(1, DriverId::allocate().unwrap());
        let id = arena.insert(Operation::Nop).unwrap();
        arena.pin(id);
        arena.abandon(id);
        arena.complete(id, Ok(Value::File(File::open("/dev/null").unwrap())));
        let record = arena.unpin(id).unwrap();
        assert!(matches!(
            record.lifecycle,
            Lifecycle::Ignored(Some(Ok(Value::File(_))))
        ));
    }
    #[test]
    fn shutdown_scan_skips_completed_records_and_preserves_external_pins() {
        let mut arena = Arena::new(2, DriverId::allocate().unwrap());
        let completed = arena.insert(Operation::Nop).unwrap();
        arena.complete(completed, Ok(Value::Done));
        let pending = arena.insert(Operation::Nop).unwrap();
        arena.pin(pending);
        arena.fail();
        let mut cursor = 0;
        assert_eq!(arena.next_cancel_target(&mut cursor), Some(pending));
        assert_eq!(arena.next_cancel_target(&mut cursor), None);
        arena.abandon(pending);
        assert!(
            arena
                .complete(pending, Err(io::Error::from_raw_os_error(libc::ECANCELED)))
                .1
                .is_none()
        );
        assert!(arena.unpin(pending).is_some());
        assert_eq!(arena.in_flight(), 0);
        assert!(arena.take(completed).is_some());
    }

    #[test]
    fn packed_ids_keep_all_24_owner_bits_and_40_slot_bits() {
        let owner = DriverId::allocate().unwrap();
        let id = OperationId::new(owner, MAX_INDEX as usize).unwrap();
        assert_eq!(id.raw(), (u64::from(owner.raw()) << INDEX_BITS) | MAX_INDEX);
        assert_eq!(id.index() as u64, MAX_INDEX);
        assert!(id.belongs_to(owner));
        assert!(!id.belongs_to(DriverId::allocate().unwrap()));
        assert!(OperationId::new(owner, (MAX_INDEX + 1) as usize).is_none());
        assert_eq!(
            OperationId::new(owner, 0).unwrap().raw(),
            u64::from(owner.raw()) << INDEX_BITS
        );
    }
}
