#![forbid(unsafe_code)]
use super::*;
use crate::io::Buffer;
use std::{cell::Cell, os::fd::AsRawFd, task::Context};

fn write(file: &Rc<Fd>, byte: u8, offset: u64) -> Operation {
    Operation::WriteAt {
        file: file.clone(),
        buffer: Buffer::from_slice(&[byte], 1).unwrap(),
        offset,
    }
}

#[test]
fn stalled_publication_is_bounded_and_returns_unpublished_inputs() {
    for error in [
        None,
        Some(libc::EINTR),
        Some(libc::EAGAIN),
        Some(libc::EBUSY),
    ] {
        let mut driver = Driver::new(1).unwrap();
        let io = driver.io().clone();
        let mut first = io.push(Operation::Nop).unwrap();
        io.shared
            .submit_faults
            .borrow_mut()
            .extend((0..3).map(|_| SubmitFault { count: 0, error }));
        let file = Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
        let operation = write(&file, b'X', 0);
        let ptr = operation.buffer().unwrap().as_ptr();
        let rejected = io.push(operation).unwrap_err();
        assert_eq!(rejected.error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(rejected.operation.buffer().unwrap().as_ptr(), ptr);
        assert_eq!(
            io.shared.submit_faults.borrow().len(),
            1,
            "exactly two attempts"
        );
        assert!(!io.is_failed());
        assert_eq!(io.in_flight(), 1);
        assert!(io.has_pending());
        io.shared.submit_faults.borrow_mut().clear();
        driver.drain().unwrap();
        first.try_complete(&io).unwrap().result.unwrap();
        assert_eq!(file.as_file().metadata().unwrap().len(), 0);
    }
}

#[test]
fn partial_submissions_and_errors_preserve_every_published_request() {
    for error in [None, Some(libc::EINTR), Some(libc::EIO)] {
        let mut driver = Driver::new(2).unwrap();
        let io = driver.io().clone();
        let original = tempfile::NamedTempFile::new().unwrap();
        let file = Rc::new(Fd::new(original.reopen().unwrap()).unwrap());
        let mut first = io.push(write(&file, b'A', 0)).unwrap();
        let mut second = io.push(write(&file, b'B', 1)).unwrap();
        io.shared
            .submit_faults
            .borrow_mut()
            .push_back(SubmitFault { count: 1, error });
        let third = io.push(write(&file, b'C', 2));
        let mut third = if error == Some(libc::EIO) {
            let rejected = third.unwrap_err();
            assert_eq!(rejected.error.raw_os_error(), error);
            assert_eq!(&**rejected.operation.buffer().unwrap(), b"C");
            assert!(io.is_failed());
            assert_eq!(
                io.push(Operation::Nop).unwrap_err().error.kind(),
                io::ErrorKind::BrokenPipe
            );
            None
        } else {
            assert!(!io.is_failed());
            Some(third.unwrap())
        };
        assert!(io.has_pending(), "the unsubmitted tail stays in the SQ");
        driver.drain().unwrap();
        for handle in [&mut first, &mut second] {
            assert_eq!(
                handle
                    .try_complete(&io)
                    .unwrap()
                    .result
                    .unwrap()
                    .into_count(),
                1
            );
        }
        if let Some(third) = &mut third {
            assert_eq!(
                third
                    .try_complete(&io)
                    .unwrap()
                    .result
                    .unwrap()
                    .into_count(),
                1
            );
            assert_eq!(std::fs::read(original.path()).unwrap(), b"ABC");
        } else {
            assert_eq!(std::fs::read(original.path()).unwrap(), b"AB");
        }
        assert_eq!(io.in_flight(), 0);
    }
}

#[test]
fn rejected_close_retains_its_descriptor_until_the_rejection_is_dropped() {
    let mut driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let mut first = io.push(Operation::Nop).unwrap();
    io.shared
        .submit_faults
        .borrow_mut()
        .extend((0..2).map(|_| SubmitFault {
            count: 0,
            error: None,
        }));
    let fd = Fd::new(tempfile::tempfile().unwrap()).unwrap();
    let raw = fd.as_raw_fd();
    let rejected = io.push(Operation::close(fd)).unwrap_err();
    assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_ok());
    drop(rejected);
    assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_err());
    driver.drain().unwrap();
    first.try_complete(&io).unwrap().result.unwrap();
}

#[test]
fn pressure_callbacks_finish_before_the_new_operation_is_published() {
    struct Count(Rc<Cell<usize>>);
    impl Drop for Count {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    struct Reenter {
        io: Io,
        called: Rc<Cell<bool>>,
    }
    impl Drop for Reenter {
        fn drop(&mut self) {
            // Only first and filler exist. The incoming operation must not
            // enter the arena until pressure callbacks have finished.
            assert_eq!(self.io.shared.state.borrow().arena.len(), 2);
            // Refill the SQ: the outer publisher must recheck space afterward.
            for _ in 0..4 {
                let mut handle = self.io.push(Operation::Nop).unwrap();
                handle.release(&self.io);
            }
            self.called.set(true);
        }
    }
    let mut driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let called = Rc::new(Cell::new(false));
    let drops = Rc::new(Cell::new(0));
    let mut first = io
        .push_with_guard(
            Operation::Nop,
            Reenter {
                io: io.clone(),
                called: called.clone(),
            },
        )
        .unwrap();
    // Make its CQE available without reaping, then fill the SQ.
    driver.submit_and_wait(None).unwrap();
    let mut filler = io.push(Operation::Nop).unwrap();
    let mut guarded = io
        .push_with_guard(Operation::Nop, Count(drops.clone()))
        .unwrap();
    assert!(called.get());
    assert_eq!(drops.get(), 0, "new guard is still awaiting completion");
    assert!(
        guarded.try_complete(&io).is_none(),
        "no reaping after publication"
    );
    driver.drain().unwrap();
    assert_eq!(drops.get(), 1);
    for handle in [&mut first, &mut filler, &mut guarded] {
        handle.try_complete(&io).unwrap().result.unwrap();
    }
}

#[test]
fn pressure_delivers_wakers_outside_borrows_and_reentrant_cancellation_is_safe() {
    thread_local! {
        static CALLBACK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }
    struct Reenter;
    impl std::task::Wake for Reenter {
        fn wake(self: std::sync::Arc<Self>) {
            let callback = CALLBACK.with(|slot| slot.borrow_mut().take());
            if let Some(callback) = callback {
                callback();
            }
        }
    }
    let mut driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let mut first = io.push(Operation::Nop).unwrap();
    let target = first.cancel_target(&io);
    let called = Rc::new(Cell::new(false));
    let output = called.clone();
    let callback_io = io.clone();
    CALLBACK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            // The completed target remains pinned while cancellation is published.
            let mut cancel = callback_io.push(Operation::Cancel(target)).unwrap();
            cancel.release(&callback_io);
            output.set(true);
        }))
    });
    let waker = Waker::from(std::sync::Arc::new(Reenter));
    assert!(
        first
            .poll_complete(&io, &mut Context::from_waker(&waker))
            .is_pending()
    );
    driver.submit_and_wait(None).unwrap();
    let mut filler = io.push(Operation::Nop).unwrap();
    let mut last = io.push(Operation::Nop).unwrap();
    assert!(called.get());
    driver.drain().unwrap();
    for handle in [&mut first, &mut filler, &mut last] {
        handle.try_complete(&io).unwrap().result.unwrap();
    }
}

#[test]
fn reentrant_borrows_reject_without_dropping_owned_inputs() {
    let driver = Driver::new(1).unwrap();
    let io = driver.io();
    let file = Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let operation = write(&file, b'A', 0);
    let ptr = operation.buffer().unwrap().as_ptr();
    let ring = io.shared.state.borrow_mut();
    let rejected = io.push(operation).unwrap_err();
    assert_eq!(rejected.error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(rejected.operation.buffer().unwrap().as_ptr(), ptr);
    drop(ring);
    let arena = io.shared.state.borrow_mut();
    let rejected = io.push(rejected.operation).unwrap_err();
    assert_eq!(rejected.operation.buffer().unwrap().as_ptr(), ptr);
    drop(arena);
}

#[test]
fn pressure_callback_closing_admission_rejects_the_unpublished_request() {
    struct CloseAdmission(Io);
    impl Drop for CloseAdmission {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let mut driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let mut first = io
        .push_with_guard(Operation::Nop, CloseAdmission(io.clone()))
        .unwrap();
    driver.submit_and_wait(None).unwrap();
    let mut filler = io.push(Operation::Nop).unwrap();
    let file = Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let operation = write(&file, b'X', 0);
    let ptr = operation.buffer().unwrap().as_ptr();
    let rejected = io.push(operation).unwrap_err();
    assert_eq!(rejected.error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(rejected.operation.buffer().unwrap().as_ptr(), ptr);
    assert_eq!(io.shared.state.borrow().arena.len(), 2);
    driver.drain().unwrap();
    first.try_complete(&io).unwrap().result.unwrap();
    filler.try_complete(&io).unwrap().result.unwrap();
    assert_eq!(file.as_file().metadata().unwrap().len(), 0);
}

// Wait for all Nops to reach the CQ without retiring their lifecycle records.
fn complete_nops(io: &Io, count: usize) {
    io.shared
        .state
        .borrow_mut()
        .ring
        .as_mut()
        .unwrap()
        .submit_and_wait(count)
        .unwrap();
}

#[test]
fn tick_delivers_each_guard_before_retiring_the_next_cqe() {
    struct Observe {
        io: Io,
        observations: Rc<RefCell<Vec<usize>>>,
    }
    impl Drop for Observe {
        fn drop(&mut self) {
            self.observations.borrow_mut().push(self.io.in_flight());
        }
    }
    let mut driver = Driver::new(2).unwrap();
    let io = driver.io().clone();
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut handles: Vec<_> = (0..2)
        .map(|_| {
            io.push_with_guard(
                Operation::Nop,
                Observe {
                    io: io.clone(),
                    observations: observations.clone(),
                },
            )
            .unwrap()
        })
        .collect();
    complete_nops(&io, 2);
    assert_eq!(driver.tick(), 2);
    assert_eq!(*observations.borrow(), [1, 0]);
    for handle in &mut handles {
        handle.try_complete(&io).unwrap().result.unwrap();
    }
}

#[test]
fn nested_tick_consumes_remaining_cqes_without_duplicate_retirement() {
    struct Reenter {
        io: Io,
        nested: Rc<Cell<usize>>,
    }
    impl Drop for Reenter {
        fn drop(&mut self) {
            self.nested.set(self.nested.get() + self.io.shared.tick());
        }
    }
    let mut driver = Driver::new(2).unwrap();
    let io = driver.io().clone();
    let nested = Rc::new(Cell::new(0));
    let mut handles: Vec<_> = (0..2)
        .map(|_| {
            io.push_with_guard(
                Operation::Nop,
                Reenter {
                    io: io.clone(),
                    nested: nested.clone(),
                },
            )
            .unwrap()
        })
        .collect();
    complete_nops(&io, 2);
    assert_eq!(driver.tick(), 1);
    assert_eq!(nested.get(), 1);
    assert_eq!(io.in_flight(), 0);
    for handle in &mut handles {
        handle.try_complete(&io).unwrap().result.unwrap();
    }
}

#[test]
fn tick_rechecks_ring_after_a_callback_destroys_the_driver() {
    struct Destroy(Rc<RefCell<Option<Driver>>>);
    impl Drop for Destroy {
        fn drop(&mut self) {
            let driver = self.0.borrow_mut().take();
            drop(driver);
        }
    }
    let driver = Driver::new(2).unwrap();
    let io = driver.io().clone();
    let owner = Rc::new(RefCell::new(Some(driver)));
    let mut handles: Vec<_> = (0..2)
        .map(|_| {
            io.push_with_guard(Operation::Nop, Destroy(owner.clone()))
                .unwrap()
        })
        .collect();
    complete_nops(&io, 2);
    // Use the shared driving path, as publication does; no borrow of the owner
    // may span this callback. Teardown consumes the remaining CQE itself.
    assert_eq!(io.shared.tick(), 1);
    assert!(owner.borrow().is_none());
    assert_eq!(io.in_flight(), 0);
    assert_eq!(
        io.push(Operation::Nop).unwrap_err().error.kind(),
        io::ErrorKind::BrokenPipe
    );
    for handle in &mut handles {
        handle.try_complete(&io).unwrap().result.unwrap();
    }
}

#[test]
fn tick_budget_excludes_work_published_by_its_callback() {
    struct Refill {
        io: Io,
        next: Rc<RefCell<Option<OperationHandle>>>,
    }
    impl Drop for Refill {
        fn drop(&mut self) {
            let handle = self.io.push(Operation::Nop).unwrap();
            complete_nops(&self.io, 1);
            *self.next.borrow_mut() = Some(handle);
        }
    }
    let mut driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let next = Rc::new(RefCell::new(None));
    let mut first = io
        .push_with_guard(
            Operation::Nop,
            Refill {
                io: io.clone(),
                next: next.clone(),
            },
        )
        .unwrap();
    complete_nops(&io, 1);
    assert_eq!(driver.tick(), 1);
    first.try_complete(&io).unwrap().result.unwrap();
    assert_eq!(io.in_flight(), 1);
    let mut next = next.borrow_mut().take().unwrap();
    assert!(next.try_complete(&io).is_none());
    assert_eq!(driver.tick(), 1);
    next.try_complete(&io).unwrap().result.unwrap();
}
