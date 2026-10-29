//! Operation slab ownership, admission, and packed slot identities.
#![forbid(unsafe_code)]
use crate::io::{Value, driver::DriverId, lifecycle::Lifecycle};
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

/// Metadata needed without borrowing the submitter's concrete operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Done,
    Count,
    File,
    Poll,
    Close,
    Cancel,
}

pub(crate) struct Record {
    lifecycle: Lifecycle,
    kernel_refs: usize,
    pub(crate) dependency: Option<OperationId>,
    pub(crate) kind: Kind,
    own_done: bool,
    pub(crate) retain_data: bool,
}

/// Resources extracted under a borrow and destroyed/notified only after it ends.
#[derive(Default)]
pub(crate) struct Retired {
    pub(crate) wake: Option<Waker>,
    pub(crate) data: Option<Box<dyn std::any::Any>>,
    pub(crate) result: Option<io::Result<Value>>,
}

pub(crate) struct Arena {
    driver_id: DriverId,
    entries: Slab<Record>,
    active: usize,
    closed: bool,
    failed: bool,
}
impl Arena {
    pub(crate) fn new(capacity: usize, driver_id: DriverId) -> Self {
        Self {
            driver_id,
            entries: Slab::with_capacity(capacity),
            active: 0,
            closed: false,
            failed: false,
        }
    }
    pub(crate) fn check_admission(&self, cancel: bool) -> io::Result<()> {
        if self.failed || (self.closed && !cancel) {
            Err(io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }
    pub(crate) fn insert(&mut self, kind: Kind, retain_data: bool) -> io::Result<OperationId> {
        self.check_admission(kind == Kind::Cancel)?;
        let entry = self.entries.vacant_entry();
        let id = OperationId::new(self.driver_id, entry.key()).ok_or(io::ErrorKind::WouldBlock)?;
        entry.insert(Record {
            lifecycle: Lifecycle::Submitted,
            kernel_refs: 1,
            dependency: None,
            kind,
            own_done: false,
            retain_data,
        });
        self.active += 1;
        Ok(id)
    }
    pub(crate) fn record(&mut self, id: OperationId) -> &mut Record {
        self.entries
            .get_mut(id.index())
            .expect("live operation identity")
    }
    pub(crate) fn reject(&mut self, id: OperationId) {
        let record = self.entries.remove(id.index());
        assert!(matches!(record.lifecycle, Lifecycle::Submitted));
        assert_eq!(record.kernel_refs, 1);
        assert!(record.dependency.is_none());
        self.active -= 1;
    }
    pub(crate) fn pin(&mut self, id: OperationId) {
        let record = self.record(id);
        record.kernel_refs = record
            .kernel_refs
            .checked_add(1)
            .expect("operation reference overflow");
    }
    pub(crate) fn unpin(&mut self, id: OperationId) {
        let record = self.record(id);
        assert!(record.kernel_refs != 0);
        record.kernel_refs -= 1;
        self.remove_empty(id);
    }
    fn remove_empty(&mut self, id: OperationId) {
        let record = self.record(id);
        if record.kernel_refs == 0 && matches!(record.lifecycle, Lifecycle::VacantPinned) {
            // Empty identity tombstones have no user resources or callbacks.
            self.entries.remove(id.index());
        }
    }
    pub(crate) fn complete(&mut self, id: OperationId, result: io::Result<Value>) -> Retired {
        let record = self.record(id);
        assert!(!record.own_done, "duplicate terminal CQE");
        record.own_done = true;
        let previous = std::mem::replace(&mut record.lifecycle, Lifecycle::VacantPinned);
        let effects = match previous {
            Lifecycle::Submitted => {
                record.lifecycle = Lifecycle::Completed(result);
                Retired::default()
            }
            Lifecycle::Waiting(wake) => {
                record.lifecycle = Lifecycle::Completed(result);
                Retired {
                    wake: Some(wake),
                    ..Retired::default()
                }
            }
            Lifecycle::Ignored(data) => Retired {
                data,
                result: Some(result),
                wake: None,
            },
            _ => unreachable!("invalid completion state"),
        };
        self.active -= 1;
        self.unpin(id);
        effects
    }
    pub(crate) fn take(&mut self, id: OperationId) -> Option<io::Result<Value>> {
        let record = self.record(id);
        if !matches!(record.lifecycle, Lifecycle::Completed(_)) {
            return None;
        }
        let Lifecycle::Completed(result) =
            std::mem::replace(&mut record.lifecycle, Lifecycle::VacantPinned)
        else {
            unreachable!()
        };
        self.remove_empty(id);
        Some(result)
    }
    pub(crate) fn wait(&mut self, id: OperationId, waker: Waker) -> Option<Waker> {
        match &mut self.record(id).lifecycle {
            state @ Lifecycle::Submitted => {
                *state = Lifecycle::Waiting(waker);
                None
            }
            Lifecycle::Waiting(old) => Some(std::mem::replace(old, waker)),
            Lifecycle::Completed(_) => Some(waker),
            _ => unreachable!("submitter released operation"),
        }
    }
    /// No callbacks occur between observing completion and transferring inputs.
    /// Completed inputs stay with the owner for destruction outside this borrow.
    pub(crate) fn abandon<T: 'static>(&mut self, id: OperationId, data: &mut Option<T>) -> Retired {
        let record = self.record(id);
        let next = if record.own_done {
            Lifecycle::VacantPinned
        } else {
            Lifecycle::Ignored(if record.retain_data {
                data.take()
                    .map(|data| Box::new(data) as Box<dyn std::any::Any>)
            } else {
                None
            })
        };
        let previous = std::mem::replace(&mut record.lifecycle, next);
        let effects = match previous {
            Lifecycle::Submitted => Retired::default(),
            Lifecycle::Waiting(wake) => Retired {
                wake: Some(wake),
                ..Retired::default()
            },
            Lifecycle::Completed(result) => Retired {
                result: Some(result),
                ..Retired::default()
            },
            _ => unreachable!("submitter released twice"),
        };
        self.remove_empty(id);
        effects
    }
    pub(crate) fn close(&mut self) {
        self.closed = true;
    }
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
    /// Teardown has closed all admission, so scanned identities cannot be reused.
    pub(crate) fn next_cancel_target(&self, cursor: &mut usize) -> Option<OperationId> {
        assert!(self.failed);
        while *cursor < self.entries.capacity() {
            let index = *cursor;
            *cursor += 1;
            if let Some(record) = self.entries.get(index)
                && !record.own_done
                && !matches!(record.kind, Kind::Cancel | Kind::Close)
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
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    fn arena() -> Arena {
        Arena::new(1, DriverId::allocate().unwrap())
    }
    struct Count(Rc<Cell<usize>>);
    impl Drop for Count {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn abandonment_retains_inputs_only_until_original_completion_even_with_pins() {
        let mut arena = arena();
        let id = arena.insert(Kind::Count, true).unwrap();
        arena.pin(id);
        let drops = Rc::new(Cell::new(0));
        let mut data = Some(Count(drops.clone()));
        drop(arena.abandon(id, &mut data));
        assert!(data.is_none());
        assert_eq!(drops.get(), 0);
        let effects = arena.complete(id, Ok(Value::Count(1)));
        assert_eq!(drops.get(), 0);
        drop(effects);
        assert_eq!(drops.get(), 1);
        assert_eq!(arena.len(), 1);
        assert!(matches!(
            arena.record(id).lifecycle,
            Lifecycle::VacantPinned
        ));
        let other = arena.insert(Kind::Done, false).unwrap();
        assert_ne!(id, other);
        arena.unpin(id);
        assert_eq!(arena.len(), 1);
        drop(arena.complete(other, Ok(Value::Done)));
        arena.take(other).unwrap().unwrap();
    }
    #[test]
    fn completed_drop_never_moves_data_into_an_abandonment_box() {
        for pinned in [false, true] {
            let mut arena = arena();
            let id = arena.insert(Kind::Count, true).unwrap();
            if pinned {
                arena.pin(id);
            }
            drop(arena.complete(id, Ok(Value::Count(1))));
            let mut data = Some(vec![1, 2, 3]);
            let effects = arena.abandon(id, &mut data);
            assert!(data.is_some());
            assert!(effects.data.is_none());
            assert!(effects.result.is_some());
            assert_eq!(arena.len(), usize::from(pinned));
            if pinned {
                arena.unpin(id);
            }
        }
    }
    #[test]
    fn consumed_result_leaves_only_identity_until_all_pins_retire() {
        let mut arena = arena();
        let id = arena.insert(Kind::Done, false).unwrap();
        arena.pin(id);
        arena.pin(id);
        drop(arena.complete(id, Ok(Value::Done)));
        arena.take(id).unwrap().unwrap();
        arena.unpin(id);
        assert_eq!(arena.len(), 1);
        arena.unpin(id);
        assert_eq!(arena.len(), 0);
        assert_eq!(arena.insert(Kind::Done, false).unwrap(), id);
    }
    #[test]
    fn cancellation_retiring_first_does_not_release_original_inputs() {
        let mut arena = arena();
        let id = arena.insert(Kind::Count, true).unwrap();
        arena.pin(id);
        let mut data = Some(vec![1]);
        drop(arena.abandon(id, &mut data));
        arena.unpin(id);
        assert_eq!(arena.in_flight(), 1);
        assert!(matches!(
            arena.record(id).lifecycle,
            Lifecycle::Ignored(Some(_))
        ));
        drop(arena.complete(id, Ok(Value::Count(1))));
        assert_eq!(arena.len(), 0);
    }
    #[test]
    fn abandoned_destructors_can_reenter_after_retirement() {
        struct Reenter(Rc<RefCell<Arena>>);
        impl Drop for Reenter {
            fn drop(&mut self) {
                self.0.borrow_mut().close();
            }
        }
        let arena = Rc::new(RefCell::new(arena()));
        let id = arena.borrow_mut().insert(Kind::Done, true).unwrap();
        let mut data = Some(Reenter(arena.clone()));
        let effects = arena.borrow_mut().abandon(id, &mut data);
        drop(effects);
        let effects = arena.borrow_mut().complete(id, Ok(Value::Done));
        drop(effects);
        assert_eq!(
            arena
                .borrow_mut()
                .insert(Kind::Done, false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }
    #[test]
    fn cancellation_and_close_need_no_abandonment_box() {
        for kind in [Kind::Cancel, Kind::Close, Kind::Done] {
            let mut arena = arena();
            let id = arena.insert(kind, false).unwrap();
            let mut data = Some(17);
            drop(arena.abandon(id, &mut data));
            assert_eq!(data, Some(17));
            assert!(matches!(
                arena.record(id).lifecycle,
                Lifecycle::Ignored(None)
            ));
            drop(arena.complete(id, Ok(Value::Done)));
            assert_eq!(arena.len(), 0);
        }
    }
    #[test]
    fn rollback_and_shutdown_scan_preserve_admission_and_pins() {
        let mut arena = arena();
        let rollback = arena.insert(Kind::Done, false).unwrap();
        arena.reject(rollback);
        assert_eq!(arena.in_flight(), 0);
        let target = arena.insert(Kind::Poll, true).unwrap();
        let close = arena.insert(Kind::Close, false).unwrap();
        arena.close();
        assert!(arena.insert(Kind::Done, false).is_err());
        let cancel = arena.insert(Kind::Cancel, false).unwrap();
        arena.fail();
        let mut cursor = 0;
        assert_eq!(arena.next_cancel_target(&mut cursor), Some(target));
        assert!(arena.next_cancel_target(&mut cursor).is_none());
        for id in [target, close, cancel] {
            drop(arena.complete(id, Ok(Value::Done)));
            arena.take(id).unwrap().unwrap();
        }
        assert_eq!(arena.len(), 0);
    }
    #[test]
    fn packed_ids_keep_all_owner_and_index_bits() {
        let owner = DriverId::allocate().unwrap();
        let id = OperationId::new(owner, MAX_INDEX as usize).unwrap();
        assert!(id.belongs_to(owner));
        assert_eq!(id.index(), MAX_INDEX as usize);
        assert!(OperationId::new(owner, MAX_INDEX as usize + 1).is_none());
        assert_eq!(size_of::<Option<OperationId>>(), 8);
    }
    #[test]
    fn layouts_include_data_moved_out_of_the_arena() {
        use crate::io::{Op, Operation, ReadAt};
        assert_eq!(size_of::<Record>(), 48);
        assert_eq!(size_of::<Lifecycle>(), 24);
        assert_eq!(size_of::<Op<ReadAt>>(), 72);
        assert_eq!(size_of::<Op<Operation>>(), 88);
        println!(
            "record={} typed_read_owner={} enum_owner={}",
            size_of::<Record>(),
            size_of::<Op<ReadAt>>(),
            size_of::<Op<Operation>>()
        );
    }
}
