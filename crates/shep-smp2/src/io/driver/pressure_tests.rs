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
        first.try_complete().unwrap().result.unwrap();
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
                handle.try_complete().unwrap().result.unwrap().into_count(),
                1
            );
        }
        if let Some(third) = &mut third {
            assert_eq!(
                third.try_complete().unwrap().result.unwrap().into_count(),
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
    first.try_complete().unwrap().result.unwrap();
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
                handle.release();
            }
            self.called.set(true);
        }
    }
    let mut driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let called = Rc::new(Cell::new(false));
    let drops = Rc::new(Cell::new(0));
    let mut first = on_complete(
        &io,
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
    let mut newest = on_complete(&io, Operation::Nop, Count(drops.clone())).unwrap();
    assert!(called.get());
    assert_eq!(drops.get(), 0, "new waiter is still awaiting completion");
    assert!(
        newest.try_complete().is_none(),
        "no reaping after publication"
    );
    driver.drain().unwrap();
    assert_eq!(drops.get(), 1);
    for handle in [&mut first, &mut filler, &mut newest] {
        handle.try_complete().unwrap().result.unwrap();
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
    let target = first.cancel_target();
    let called = Rc::new(Cell::new(false));
    let output = called.clone();
    let callback_io = io.clone();
    CALLBACK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            // The completed target remains pinned while cancellation is published.
            let mut cancel = callback_io.push(Operation::Cancel(target)).unwrap();
            cancel.release();
            output.set(true);
        }))
    });
    let waker = Waker::from(std::sync::Arc::new(Reenter));
    assert!(
        first
            .poll_complete(&mut Context::from_waker(&waker))
            .is_pending()
    );
    driver.submit_and_wait(None).unwrap();
    let mut filler = io.push(Operation::Nop).unwrap();
    let mut last = io.push(Operation::Nop).unwrap();
    assert!(called.get());
    driver.drain().unwrap();
    for handle in [&mut first, &mut filler, &mut last] {
        handle.try_complete().unwrap().result.unwrap();
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
    let mut first = on_complete(&io, Operation::Nop, CloseAdmission(io.clone())).unwrap();
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
    first.try_complete().unwrap().result.unwrap();
    filler.try_complete().unwrap().result.unwrap();
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
fn tick_delivers_each_waiter_before_retiring_the_next_cqe() {
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
            on_complete(
                &io,
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
        handle.try_complete().unwrap().result.unwrap();
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
            on_complete(
                &io,
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
        handle.try_complete().unwrap().result.unwrap();
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
        .map(|_| on_complete(&io, Operation::Nop, Destroy(owner.clone())).unwrap())
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
        handle.try_complete().unwrap().result.unwrap();
    }
}

#[test]
fn tick_budget_excludes_work_published_by_its_callback() {
    struct Refill {
        io: Io,
        next: Rc<RefCell<Option<Op<Operation>>>>,
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
    let mut first = on_complete(
        &io,
        Operation::Nop,
        Refill {
            io: io.clone(),
            next: next.clone(),
        },
    )
    .unwrap();
    complete_nops(&io, 1);
    assert_eq!(driver.tick(), 1);
    first.try_complete().unwrap().result.unwrap();
    assert_eq!(io.in_flight(), 1);
    let mut next = next.borrow_mut().take().unwrap();
    assert!(next.try_complete().is_none());
    assert_eq!(driver.tick(), 1);
    next.try_complete().unwrap().result.unwrap();
}

// Exercise callbacks through the supported waiter API, not completion guards.
fn on_complete<G: 'static>(io: &Io, operation: Operation, callback: G) -> Result<Op, Rejected> {
    type Callback = Option<Box<dyn FnOnce()>>;
    thread_local! {
        static CALLBACKS: RefCell<Vec<Callback>> = RefCell::default();
    }
    struct Notify(usize);
    impl std::task::Wake for Notify {
        fn wake(self: std::sync::Arc<Self>) {
            let callback = CALLBACKS.with(|callbacks| callbacks.borrow_mut()[self.0].take());
            if let Some(callback) = callback {
                callback();
            }
        }
    }
    let mut op = io.push(operation)?;
    let index = CALLBACKS.with(|callbacks| {
        let mut callbacks = callbacks.borrow_mut();
        let index = callbacks.len();
        callbacks.push(Some(Box::new(move || drop(callback))));
        index
    });
    let wake = Waker::from(std::sync::Arc::new(Notify(index)));
    assert!(
        op.poll_complete(&mut Context::from_waker(&wake))
            .is_pending()
    );
    Ok(op)
}

#[test]
fn dropping_replaced_waker_can_reap_the_operation_before_poll_returns() {
    thread_local! {
        static CALLBACK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::default();
    }
    struct ReenterOnDrop;
    // This waker exists to exercise its destructor, not its wake method.
    #[allow(clippy::manual_noop_waker)]
    impl std::task::Wake for ReenterOnDrop {
        fn wake(self: std::sync::Arc<Self>) {}
    }
    impl Drop for ReenterOnDrop {
        fn drop(&mut self) {
            let callback = CALLBACK.with(|slot| slot.borrow_mut().take());
            if let Some(callback) = callback {
                callback();
            }
        }
    }
    let driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let mut op = io.push(Operation::Nop).unwrap();
    let callback_io = io.clone();
    CALLBACK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            assert_eq!(callback_io.shared.tick(), 1);
        }))
    });
    let waker = Waker::from(std::sync::Arc::new(ReenterOnDrop));
    assert!(
        op.poll_complete(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker); // the arena now holds the final reference
    complete_nops(&io, 1);
    // Replacement drops the old waker outside the borrow; its Drop reaps this
    // operation. The post-callback recheck must observe that completion.
    assert!(
        op.poll_complete(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    assert_eq!(io.in_flight(), 0);
}

fn network_pair(io: &Io) -> (crate::net::TcpStream, std::net::TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    let stream = crate::net::TcpStream::from_std(io, client, Default::default()).unwrap();
    (stream, peer)
}

#[test]
fn network_cancellation_submits_once_then_retries_after_admission_closes() {
    use std::{future::Future, pin::Pin, task::Waker};
    for error in [
        None,
        Some(libc::EINTR),
        Some(libc::EAGAIN),
        Some(libc::EBUSY),
    ] {
        let mut driver = Driver::new(1).unwrap();
        let io = driver.io().clone();
        let (mut stream, _peer) = network_pair(&io);
        let buffer = Buffer::new(1, 64).unwrap();
        let ptr = buffer.as_ptr();
        let mut read = stream.read(buffer);
        let mut cx = Context::from_waker(Waker::noop());
        assert!(Pin::new(&mut read).poll(&mut cx).is_pending());
        io.close();
        io.shared
            .submit_faults
            .borrow_mut()
            .push_back(SubmitFault { count: 1, error });
        let mut cancellation = Box::pin(read.cancel());
        assert!(cancellation.as_mut().poll(&mut cx).is_pending());
        assert!(io.shared.submit_faults.borrow().is_empty());
        assert_eq!(io.in_flight(), 2, "original and published cancellation");
        driver.drain().unwrap();
        let std::task::Poll::Ready((result, buffer)) = cancellation.as_mut().poll(&mut cx) else {
            panic!("canceled receive must have retired");
        };
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ECANCELED));
        assert_eq!(buffer.as_ptr(), ptr);
        assert_eq!(io.in_flight(), 0);
    }
}

#[test]
fn rejected_network_cancellation_has_no_backlog_and_awaits_natural_completion() {
    use std::{future::Future, io::Write, pin::Pin, task::Waker, time::Duration};
    for error in [
        None,
        Some(libc::EINTR),
        Some(libc::EAGAIN),
        Some(libc::EBUSY),
    ] {
        let mut driver = Driver::new(1).unwrap();
        let io = driver.io().clone();
        let (mut stream, mut peer) = network_pair(&io);
        let buffer = Buffer::new(1, 64).unwrap();
        let ptr = buffer.as_ptr();
        let mut read = stream.read(buffer);
        let mut cx = Context::from_waker(Waker::noop());
        assert!(Pin::new(&mut read).poll(&mut cx).is_pending());
        io.shared
            .submit_faults
            .borrow_mut()
            .push_back(SubmitFault { count: 0, error });
        let mut cancellation = Box::pin(read.cancel());
        assert!(cancellation.as_mut().poll(&mut cx).is_pending());
        assert!(io.shared.submit_faults.borrow().is_empty());
        assert_eq!(io.in_flight(), 1, "second publication attempt failed");
        driver.submit().unwrap();
        driver
            .submit_and_wait(Some(Instant::now() + Duration::from_millis(5)))
            .unwrap();
        driver.tick();
        assert!(!io.has_pending(), "there is no deferred cancellation");
        assert_eq!(io.in_flight(), 1, "the receive is still live");
        assert!(cancellation.as_mut().poll(&mut cx).is_pending());
        peer.write_all(b"X").unwrap();
        driver.drain().unwrap();
        let std::task::Poll::Ready((result, buffer)) = cancellation.as_mut().poll(&mut cx) else {
            panic!("natural completion must return the original buffer");
        };
        assert_eq!(result.unwrap(), 1);
        assert_eq!(buffer.as_ptr(), ptr);
        assert_eq!(&*buffer, b"X");
    }
}

#[test]
fn failed_cancellation_publication_releases_pin_and_preserves_fatal_error_handling() {
    for error in [None, Some(libc::EIO)] {
        let mut driver = Driver::new(1).unwrap();
        let io = driver.io().clone();
        let weak = Rc::downgrade(&io.shared);
        let mut original = io.push(Operation::Nop).unwrap();
        io.shared
            .submit_faults
            .borrow_mut()
            .push_back(SubmitFault { count: 0, error });
        let rejected = io.request_cancel(original.cancel_target()).unwrap_err();
        if error.is_some() {
            assert_eq!(rejected.raw_os_error(), error);
            assert!(io.is_failed());
        } else {
            assert_eq!(rejected.kind(), io::ErrorKind::WouldBlock);
            assert!(!io.is_failed());
        }
        assert_eq!(io.in_flight(), 1);
        original.release();
        drop(original);
        if error.is_none() {
            driver.drain().unwrap();
        }
        // Failed drivers use the existing cancel-and-retire destructor fallback.
        drop(driver);
        assert_eq!(io.in_flight(), 0);
        assert_eq!(io.shared.state.borrow().arena.len(), 0);
        drop(io);
        assert!(weak.upgrade().is_none());
    }
}

#[test]
fn runtime_shutdown_waits_for_uncanceled_read_on_surviving_socket() {
    use std::{future::Future, io::Write, pin::Pin, sync::mpsc, task::Waker, time::Duration};
    let (started, peer_ready) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut runtime = crate::Runtime::new(crate::Config {
            submission_queue_capacity: 2,
        })
        .unwrap();
        let io = runtime.io();
        let (mut stream, peer) = network_pair(&io);
        let mut read = stream.read(Buffer::new(1, 1).unwrap());
        assert!(
            Pin::new(&mut read)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        // Wake watch + receive fill the SQ. Drop cannot publish its cancellation.
        io.shared.submit_faults.borrow_mut().push_back(SubmitFault {
            count: 0,
            error: None,
        });
        drop(read);
        assert!(io.shared.submit_faults.borrow().is_empty());
        assert_eq!(io.in_flight(), 2);
        started.send(peer).unwrap();
        runtime.shutdown().unwrap();
        assert_eq!(io.in_flight(), 0);
        finished.send(()).unwrap();
        drop(stream);
    });
    let mut peer = peer_ready.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(
            done.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "shutdown must not return while the abandoned receive is pending"
    );
    peer.write_all(b"X").unwrap();
    done.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
}
