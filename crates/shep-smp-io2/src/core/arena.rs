//! Shared driver/future lifecycle. No kernel pointers are exposed here.
#![forbid(unsafe_code)]

use super::{Completion, Operation, OperationId, Rejected};
use slab::Slab;
use std::{io, task::Waker};

/// Every entry is owned by both the driver and its operation future until one
/// releases its interest. The last party to finish removes the entry.
enum Lifecycle {
    Submitted,
    Waiting(Waker),
    Ignored,
    Completed(io::Result<usize>),
}
pub(super) struct Request {
    pub(super) operation: Operation,
    lifecycle: Lifecycle,
}

pub(super) struct Arena {
    entries: Slab<Request>,
    active: usize,
    capacity: usize,
    closed: bool,
    failed: bool,
}
impl Arena {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            entries: Slab::with_capacity(capacity),
            active: 0,
            capacity,
            closed: false,
            failed: false,
        }
    }
    pub(super) fn insert(&mut self, operation: Operation) -> Result<OperationId, Rejected> {
        let index = self.entries.vacant_key();
        let error = if self.closed {
            Some(io::Error::new(io::ErrorKind::BrokenPipe, "reactor closed"))
        } else if self.active == self.capacity {
            Some(io::Error::from(io::ErrorKind::WouldBlock))
        } else if index as u64 >= super::REMOVE {
            Some(io::Error::other("operation slot address space exhausted"))
        } else if matches!(&operation, Operation::Read { offset, .. } | Operation::Write { offset, .. } if *offset > i64::MAX as u64)
        {
            Some(io::Error::new(
                io::ErrorKind::InvalidInput,
                "offset exceeds positional I/O range",
            ))
        } else if operation
            .buffer()
            .is_some_and(|b| b.len() > i32::MAX as usize)
        {
            Some(io::Error::new(
                io::ErrorKind::InvalidInput,
                "I/O buffer too large",
            ))
        } else {
            None
        };
        if let Some(error) = error {
            return Err(Rejected { error, operation });
        }
        let inserted = self.entries.insert(Request {
            operation,
            lifecycle: Lifecycle::Submitted,
        });
        debug_assert_eq!(index, inserted);
        self.active += 1;
        Ok(OperationId::new(index))
    }
    pub(super) fn get_mut(&mut self, token: OperationId) -> &mut Request {
        self.entries
            .get_mut(token.index())
            .expect("live operation token")
    }
    /// Roll back only when SQ publication failed: no kernel reference exists.
    pub(super) fn reject(&mut self, token: OperationId, error: io::Error) -> Rejected {
        let request = self.entries.remove(token.index());
        assert!(matches!(request.lifecycle, Lifecycle::Submitted));
        self.active -= 1;
        Rejected {
            operation: request.operation,
            error,
        }
    }
    pub(super) fn complete(
        &mut self,
        token: OperationId,
        result: io::Result<usize>,
    ) -> (Option<Waker>, Option<Request>) {
        let request = self.get_mut(token);
        let previous = std::mem::replace(&mut request.lifecycle, Lifecycle::Completed(result));
        let (waker, ignored) = match previous {
            Lifecycle::Submitted => (None, false),
            Lifecycle::Waiting(waker) => (Some(waker), false),
            Lifecycle::Ignored => (None, true),
            Lifecycle::Completed(_) => unreachable!("duplicate terminal CQE"),
        };
        self.active -= 1;
        (waker, ignored.then(|| self.entries.remove(token.index())))
    }
    /// Install a pre-cloned waker. The old waker must be dropped outside borrow.
    pub(super) fn wait(&mut self, token: OperationId, waker: Waker) -> Option<Waker> {
        let request = self.get_mut(token);
        match &mut request.lifecycle {
            Lifecycle::Submitted => {
                request.lifecycle = Lifecycle::Waiting(waker);
                None
            }
            Lifecycle::Waiting(old) => Some(std::mem::replace(old, waker)),
            Lifecycle::Completed(_) => Some(waker),
            Lifecycle::Ignored => unreachable!("poll after consumer release"),
        }
    }
    pub(super) fn take(&mut self, token: OperationId) -> Option<Completion> {
        if !matches!(
            self.entries.get(token.index())?.lifecycle,
            Lifecycle::Completed(_)
        ) {
            return None;
        }
        let Request {
            operation,
            lifecycle,
        } = self.entries.remove(token.index());
        let Lifecycle::Completed(result) = lifecycle else {
            unreachable!()
        };
        Some(Completion {
            id: token,
            operation,
            result,
        })
    }
    pub(super) fn abandon(&mut self, token: OperationId) -> (Option<Waker>, Option<Request>) {
        let request = self.get_mut(token);
        match &mut request.lifecycle {
            Lifecycle::Completed(_) => (None, Some(self.entries.remove(token.index()))),
            _ => {
                let previous = std::mem::replace(&mut request.lifecycle, Lifecycle::Ignored);
                let waker = match previous {
                    Lifecycle::Waiting(waker) => Some(waker),
                    Lifecycle::Submitted => None,
                    _ => unreachable!("consumer released twice"),
                };
                (waker, None)
            }
        }
    }
    pub(super) fn pending_wakers(&mut self) -> Vec<Waker> {
        let mut wakes = Vec::new();
        for (_, request) in &mut self.entries {
            if matches!(request.lifecycle, Lifecycle::Waiting(_)) {
                let Lifecycle::Waiting(waker) =
                    std::mem::replace(&mut request.lifecycle, Lifecycle::Submitted)
                else {
                    unreachable!()
                };
                wakes.push(waker);
            }
        }
        wakes
    }
    pub(super) fn in_flight(&self) -> usize {
        self.active
    }
    pub(super) fn is_failed(&self) -> bool {
        self.failed
    }
    pub(super) fn close(&mut self) {
        self.closed = true;
    }
    pub(super) fn fail(&mut self) {
        self.closed = true;
        self.failed = true;
    }
    pub(super) fn retain_unresolved(&mut self) -> Vec<Request> {
        self.fail();
        let indices: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(index, request)| {
                (!matches!(request.lifecycle, Lifecycle::Completed(_))).then_some(index)
            })
            .collect();
        let requests = indices
            .into_iter()
            .map(|index| self.entries.remove(index))
            .collect();
        self.active = 0;
        requests
    }
    #[cfg(test)]
    pub(super) fn live_entries(&self) -> usize {
        self.entries.len()
    }
    #[cfg(all(test, not(miri)))]
    pub(super) fn completed(&self) -> Vec<OperationId> {
        self.entries
            .iter()
            .filter_map(|(index, request)| {
                matches!(request.lifecycle, Lifecycle::Completed(_))
                    .then_some(OperationId::new(index))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Buffer;
    use std::{fs::File, sync::Arc};
    fn operation() -> Operation {
        Operation::Read {
            file: Arc::new(File::open("/dev/null").unwrap()),
            buffer: Buffer::new(8, 8).unwrap(),
            offset: 0,
        }
    }
    #[test]
    fn completion_first_keeps_values_until_the_future_consumes_them() {
        let mut arena = Arena::new(1);
        let token = arena.insert(operation()).unwrap();
        assert!(arena.take(token).is_none());
        let (wake, discard) = arena.complete(token, Ok(3));
        assert!(wake.is_none() && discard.is_none());
        assert_eq!(arena.in_flight(), 0);
        assert_eq!(arena.live_entries(), 1);
        let completion = arena.take(token).unwrap();
        assert_eq!(completion.result.unwrap(), 3);
        assert_eq!(arena.live_entries(), 0);
        let next = arena.insert(operation()).unwrap();
        assert_eq!(next.index(), token.index());
    }
    #[test]
    fn future_drop_first_retains_resources_until_the_terminal_cqe() {
        let mut arena = Arena::new(1);
        let operation = operation();
        let Operation::Read { file, .. } = &operation else {
            unreachable!()
        };
        let weak = Arc::downgrade(file);
        let token = arena.insert(operation).unwrap();
        assert!(arena.abandon(token).1.is_none());
        assert_eq!(arena.live_entries(), 1);
        assert_eq!(arena.in_flight(), 1);
        assert!(weak.upgrade().is_some());
        let (_, discarded) = arena.complete(token, Ok(0));
        assert_eq!(arena.live_entries(), 0);
        assert_eq!(arena.in_flight(), 0);
        drop(discarded);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn waiting_completion_extracts_waker_before_removing_a_completed_entry() {
        let mut arena = Arena::new(1);
        let token = arena.insert(operation()).unwrap();
        assert!(arena.wait(token, Waker::noop().clone()).is_none());
        assert!(arena.wait(token, Waker::noop().clone()).is_some());
        let (wake, discarded) = arena.complete(token, Ok(0));
        assert!(wake.is_some() && discarded.is_none());
        assert!(arena.take(token).is_some());
        assert_eq!(arena.live_entries(), 0);
        drop(wake); // Driver holds a waker, not an index into a reused slot.
    }
    #[test]
    fn dropping_a_waiting_future_releases_its_waker_but_not_its_operation() {
        let mut arena = Arena::new(1);
        let token = arena.insert(operation()).unwrap();
        arena.wait(token, Waker::noop().clone());
        let (wake, discarded) = arena.abandon(token);
        assert!(wake.is_some() && discarded.is_none());
        assert_eq!(arena.in_flight(), 1);
        let (wake, discarded) = arena.complete(token, Ok(0));
        assert!(wake.is_none() && discarded.is_some());
        assert_eq!(arena.live_entries(), 0);
    }
    #[test]
    fn dropping_a_completed_future_removes_the_entry() {
        let mut arena = Arena::new(1);
        let token = arena.insert(operation()).unwrap();
        arena.complete(token, Ok(0));
        assert!(arena.abandon(token).1.is_some());
        assert_eq!(arena.live_entries(), 0);
    }
    #[test]
    fn completed_results_do_not_consume_kernel_capacity() {
        let mut arena = Arena::new(1);
        let first = arena.insert(operation()).unwrap();
        arena.complete(first, Ok(0));
        let second = arena.insert(operation()).unwrap();
        assert_ne!(first.index(), second.index());
        assert_eq!(arena.live_entries(), 2);
        assert_eq!(arena.in_flight(), 1);
        assert!(arena.take(first).is_some());
        arena.complete(second, Ok(0));
        assert!(arena.take(second).is_some());
    }
    #[test]
    fn rejection_and_closure_preserve_unaccepted_values() {
        let mut arena = Arena::new(1);
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
}
