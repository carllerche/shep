#![forbid(unsafe_code)]
#![cfg(not(miri))]
use shep_smp2::io::{Buffer, Completion, Driver, Fd, Op, Operation, Value};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    net::{SocketAddr, TcpListener},
    os::fd::OwnedFd,
    sync::Arc,
    task::{Context, Waker},
};

fn finish(driver: &mut Driver, handle: &mut Op<Operation>) -> Completion {
    loop {
        driver.tick();
        if let Some(done) = handle.try_complete() {
            return done;
        }
        driver.submit_and_wait(None).unwrap();
    }
}

// Linux removes the descriptor before reporting a filesystem flush error.
// vwsim explores these outcomes for regular files by default.
fn assert_close_result(result: std::io::Result<Value>) {
    if let Err(error) = result {
        let errno = error.raw_os_error();
        assert!(
            matches!(
                errno,
                Some(libc::ECANCELED | libc::EIO | libc::ENOSPC | libc::EINTR)
            ),
            "unexpected CLOSE errno: {errno:?}"
        );
    }
}

#[test]
fn tick_delivers_each_notification_once() {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    struct Count(AtomicUsize);
    impl std::task::Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Relaxed);
        }
    }
    let mut driver = Driver::new(1).unwrap();

    let count = Arc::new(Count(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    for round in 0..3 {
        let mut handle = driver.push(Operation::Nop).unwrap();
        assert!(
            handle
                .poll_complete(&mut Context::from_waker(&waker))
                .is_pending()
        );
        let mut completed = 0;
        while driver.in_flight() != 0 {
            driver.submit_and_wait(None).unwrap();
            completed += driver.tick();
        }
        assert_eq!(completed, 1);
        assert_eq!(driver.tick(), 0);
        handle.try_complete().unwrap().result.unwrap();
        assert_eq!(count.0.load(Relaxed), round + 1);
    }

    assert_eq!(count.0.load(Relaxed), 3);
    driver.drain().unwrap();
}

#[test]
fn push_reap_and_submit_are_separate_batch_phases() {
    let mut driver = Driver::new(8).unwrap();
    let mut handles: Vec<_> = (0..8)
        .map(|_| driver.push(Operation::Nop).unwrap())
        .collect();
    assert!(driver.has_pending());
    assert_eq!(driver.tick(), 0);
    assert!(handles.iter_mut().all(|h| h.try_complete().is_none()));
    assert_eq!(driver.submit().unwrap(), 8);
    assert!(!driver.has_pending());
    for handle in &mut handles {
        finish(&mut driver, handle).result.unwrap();
    }
    driver.drain().unwrap();
}

#[test]
fn full_sq_submits_existing_work_and_preserves_buffer() {
    let mut driver = Driver::new(1).unwrap();
    let mut first = driver.push(Operation::Nop).unwrap();
    let buffer = Buffer::from_slice(b"ownership", 64).unwrap();
    let address = buffer.as_ptr();
    let mut second = driver
        .push(Operation::WriteAt {
            file: std::rc::Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap()),
            buffer,
            offset: 0,
        })
        .unwrap();
    let completion = finish(&mut driver, &mut second);
    assert_eq!(completion.result.unwrap().into_count(), 9);
    assert_eq!(completion.operation.buffer().unwrap().as_ptr(), address);
    finish(&mut driver, &mut first).result.unwrap();
    driver.drain().unwrap();
}

#[test]
fn write_at_overrides_append_mode_without_changing_shared_flags() {
    check_write_at_append(false);
}

#[test]
fn write_at_overrides_append_enabled_after_push() {
    check_write_at_append(true);
}

fn check_write_at_append(enable_after_push: bool) {
    use shep_sys::{file_flags, set_file_flags};

    let mut driver = Driver::new(1).unwrap();
    let temp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(temp.path(), b"abc").unwrap();
    let file = Arc::new(
        OpenOptions::new()
            .write(true)
            .append(!enable_after_push)
            .open(temp.path())
            .unwrap(),
    );
    let mut alias = file.try_clone().unwrap();
    let file = std::rc::Rc::new(Fd::new(Arc::try_unwrap(file).unwrap()).unwrap());
    let buffer = Buffer::from_slice(b"X", 1).unwrap();
    let ptr = buffer.as_ptr();
    let mut handle = driver
        .push(Operation::WriteAt {
            file: file.clone(),
            buffer,
            offset: 0,
        })
        .unwrap();
    if enable_after_push {
        // Deterministically exercise the check-then-submit race through a clone
        // sharing the open file description, before the kernel sees the SQE.
        let flags = file_flags(&alias).unwrap();
        assert!(flags & libc::O_APPEND == 0);
        set_file_flags(&alias, flags | libc::O_APPEND).unwrap();
    }
    let flags = file_flags(&alias).unwrap();
    assert!(flags & libc::O_APPEND != 0);
    let done = finish(&mut driver, &mut handle);
    assert_eq!(done.result.unwrap().into_count(), 1);
    assert_eq!(done.operation.buffer().unwrap().as_ptr(), ptr);
    assert_eq!(std::fs::read(temp.path()).unwrap(), b"Xbc");
    assert_eq!(alias.stream_position().unwrap(), 0);
    assert_eq!(file_flags(&alias).unwrap(), flags);
    // The override is per request; ordinary writes through the clone still append.
    alias.write_all(b"Y").unwrap();
    assert_eq!(std::fs::read(temp.path()).unwrap(), b"XbcY");
    driver.drain().unwrap();
}

#[test]
fn positional_owned_buffers_and_errors_preserve_allocation() {
    let mut driver = Driver::new(2).unwrap();
    let mut original = tempfile::tempfile().unwrap();
    let file = std::rc::Rc::new(Fd::new(original.try_clone().unwrap()).unwrap());
    let buffer = Buffer::from_slice(b"positional", 4096).unwrap();
    let ptr = buffer.as_ptr();
    let mut write = driver
        .push(Operation::WriteAt {
            file: file.clone(),
            buffer,
            offset: 9,
        })
        .unwrap();
    let done = finish(&mut driver, &mut write);
    assert_eq!(done.result.unwrap().into_count(), 10);
    assert_eq!(done.operation.buffer().unwrap().as_ptr(), ptr);
    let mut read = driver
        .push(Operation::ReadAt {
            file,
            buffer: Buffer::new(32, 64).unwrap(),
            offset: 9,
        })
        .unwrap();
    let done = finish(&mut driver, &mut read);
    assert_eq!(done.result.unwrap().into_count(), 10);
    assert_eq!(&done.operation.buffer().unwrap()[..10], b"positional");
    original.seek(SeekFrom::Start(9)).unwrap();
    let mut bytes = Vec::new();
    original.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"positional");
    let mut bad = driver
        .push(Operation::WriteAt {
            file: std::rc::Rc::new(Fd::new(File::open("/dev/null").unwrap()).unwrap()),
            buffer: done.operation.into_buffer().unwrap(),
            offset: 0,
        })
        .unwrap();
    let done = finish(&mut driver, &mut bad);
    assert!(done.result.is_err());
    assert_eq!(done.operation.into_buffer().unwrap().len(), 32);
    driver.drain().unwrap();
}

#[test]
fn cancellation_turns_over_full_sq_and_retains_target_identity() {
    let mut driver = Driver::new(1).unwrap();
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let socket = std::rc::Rc::new(Fd::new(File::from(OwnedFd::from(socket))).unwrap());
    let mut target = driver
        .push(Operation::Recv {
            socket: socket.clone(),
            buffer: Buffer::new(8, 1).unwrap(),
        })
        .unwrap();
    let mut cancel = driver
        .push(Operation::Cancel(target.cancel_target()))
        .unwrap();
    assert_eq!(driver.in_flight(), 2);
    let done = finish(&mut driver, &mut target);
    assert_eq!(
        done.result.unwrap_err().raw_os_error(),
        Some(libc::ECANCELED)
    );
    finish(&mut driver, &mut cancel).result.unwrap();
    // A retained token can outlive a consumed completion. It must never match
    // the later receive, even when the original operation has already finished.
    let mut first = driver.push(Operation::Nop).unwrap();
    let token = first.cancel_target();
    finish(&mut driver, &mut first).result.unwrap();
    let mut later = driver
        .push(Operation::Recv {
            socket,
            buffer: Buffer::new(8, 1).unwrap(),
        })
        .unwrap();
    let mut stale_cancel = driver.push(Operation::Cancel(token)).unwrap();
    assert!(finish(&mut driver, &mut stale_cancel).result.is_err());
    assert!(later.try_complete().is_none());
    let mut cancel = driver
        .push(Operation::Cancel(later.cancel_target()))
        .unwrap();
    assert!(finish(&mut driver, &mut later).result.is_err());
    finish(&mut driver, &mut cancel).result.unwrap();
    driver.drain().unwrap();
}

#[test]
fn unsubmitted_cancellation_token_releases_pin_and_other_ring_rejects_it() {
    let mut driver = Driver::new(1).unwrap();
    let other = Driver::new(1).unwrap();
    let mut op = driver.push(Operation::Nop).unwrap();
    let rejected = other
        .push(Operation::Cancel(op.cancel_target()))
        .unwrap_err();
    assert_eq!(rejected.error.kind(), std::io::ErrorKind::InvalidInput);
    drop(rejected);
    finish(&mut driver, &mut op).result.unwrap();
    driver.drain().unwrap();
}

#[test]
fn ignored_operations_keep_descriptors_until_cqe_and_results_survive_driver() {
    let mut driver = Driver::new(1).unwrap();
    let file = std::rc::Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let weak = std::rc::Rc::downgrade(&file);
    driver
        .push(Operation::WriteAt {
            file,
            buffer: Buffer::new(1, 1).unwrap(),
            offset: 0,
        })
        .unwrap()
        .release();
    assert!(weak.upgrade().is_some());
    driver.drain().unwrap();
    assert!(weak.upgrade().is_none());
    let mut driver = Driver::new(1).unwrap();
    let mut op = driver.push(Operation::Nop).unwrap();
    driver.drain().unwrap();
    drop(driver);
    assert!(op.try_complete().unwrap().result.is_ok());
}

#[test]
fn open_allocate_truncate_sync_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut driver = Driver::new(1).unwrap();
    let mut open = driver
        .push(Operation::OpenAt {
            directory: Some(std::rc::Rc::new(
                Fd::new(File::open(dir.path()).unwrap()).unwrap(),
            )),
            path: CString::new("created").unwrap(),
            flags: libc::O_CREAT | libc::O_RDWR,
            mode: 0o600,
        })
        .unwrap();
    let Value::File(file) = finish(&mut driver, &mut open).result.unwrap() else {
        panic!("fd result")
    };
    let file = std::rc::Rc::new(file);
    for operation in [
        Operation::Fallocate {
            file: file.clone(),
            offset: 0,
            len: 4096,
            mode: 0,
        },
        Operation::Ftruncate {
            file: file.clone(),
            len: 17,
        },
        Operation::Fsync {
            file: file.clone(),
            data_only: true,
        },
    ] {
        let mut op = driver.push(operation).unwrap();
        finish(&mut driver, &mut op).result.unwrap();
    }
    assert_eq!(file.as_file().metadata().unwrap().len(), 17);
    let mut close = driver
        .push(Operation::close(std::rc::Rc::try_unwrap(file).unwrap()))
        .unwrap();
    let done = finish(&mut driver, &mut close);
    assert_close_result(done.result);
    assert_eq!(descriptors_for(&dir.path().join("created")), 0);
    assert!(driver.push(done.operation).is_err());
    driver.drain().unwrap();
}

#[test]
fn socket_connect_accept_send_recv_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    let mut driver = Driver::new(4).unwrap();
    let mut create = driver
        .push(Operation::Socket {
            domain: libc::AF_INET,
            socket_type: libc::SOCK_STREAM,
            protocol: 0,
        })
        .unwrap();
    let Value::File(client) = finish(&mut driver, &mut create).result.unwrap() else {
        panic!("socket result")
    };
    let client = std::rc::Rc::new(client);
    let listener = std::rc::Rc::new(Fd::new(File::from(OwnedFd::from(listener))).unwrap());
    let mut accept = driver.push(Operation::Accept { socket: listener }).unwrap();
    let mut connect = driver
        .push(Operation::Connect {
            socket: client.clone(),
            address: address.into(),
        })
        .unwrap();
    finish(&mut driver, &mut connect).result.unwrap();
    let Value::File(server) = finish(&mut driver, &mut accept).result.unwrap() else {
        panic!("accept result")
    };
    let server = std::rc::Rc::new(server);
    let mut recv = driver
        .push(Operation::Recv {
            socket: server,
            buffer: Buffer::new(16, 1).unwrap(),
        })
        .unwrap();
    let mut send = driver
        .push(Operation::Send {
            socket: client.clone(),
            buffer: Buffer::from_slice(b"hello", 1).unwrap(),
        })
        .unwrap();
    assert_eq!(
        finish(&mut driver, &mut send).result.unwrap().into_count(),
        5
    );
    let done = finish(&mut driver, &mut recv);
    assert_eq!(done.result.unwrap().into_count(), 5);
    assert_eq!(&done.operation.buffer().unwrap()[..5], b"hello");
    let mut shutdown = driver
        .push(Operation::Shutdown {
            socket: client,
            how: std::net::Shutdown::Both,
        })
        .unwrap();
    finish(&mut driver, &mut shutdown).result.unwrap();
    driver.drain().unwrap();
}

#[test]
fn panicking_waker_does_not_strand_other_results() {
    struct Bad;
    impl std::task::Wake for Bad {
        fn wake(self: Arc<Self>) {
            panic!("bad waker");
        }
    }
    let mut driver = Driver::new(2).unwrap();
    let mut first = driver.push(Operation::Nop).unwrap();
    let mut second = driver.push(Operation::Nop).unwrap();
    let bad = Waker::from(Arc::new(Bad));
    assert!(
        first
            .poll_complete(&mut Context::from_waker(&bad))
            .is_pending()
    );
    assert!(
        second
            .poll_complete(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    driver.drain().unwrap();
    assert!(first.try_complete().unwrap().result.is_ok());
    assert!(second.try_complete().unwrap().result.is_ok());
}

#[test]
fn reentrant_wake_consumes_another_result_and_pushes_into_reused_slot() {
    use std::{cell::RefCell, rc::Rc};
    thread_local! { static CALLBACK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) }; }
    struct Reenter;
    impl std::task::Wake for Reenter {
        fn wake(self: Arc<Self>) {
            let callback = CALLBACK.with(|slot| slot.borrow_mut().take());
            callback.unwrap()();
        }
    }
    let mut driver = Driver::new(2).unwrap();
    let mut b = driver.push(Operation::Nop).unwrap();
    while driver.in_flight() != 0 {
        driver.submit_and_wait(None).unwrap();
        driver.tick();
    }
    let mut a = driver.push(Operation::Nop).unwrap();
    let next = Rc::new(RefCell::new(None));
    let output = next.clone();
    let io = driver.io().clone();
    CALLBACK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            b.try_complete()
                .expect("B retired before callback")
                .result
                .unwrap();
            *output.borrow_mut() = Some(io.push(Operation::Nop).unwrap());
        }))
    });
    let waker = Waker::from(Arc::new(Reenter));
    assert!(
        a.poll_complete(&mut Context::from_waker(&waker))
            .is_pending()
    );
    // The callback consumes an earlier completion and reuses its freed slot.

    while driver.in_flight() != 0 {
        driver.submit_and_wait(None).unwrap();
        driver.tick();
    }

    a.try_complete().unwrap().result.unwrap();
    finish(&mut driver, next.borrow_mut().as_mut().unwrap())
        .result
        .unwrap();
    driver.drain().unwrap();
}

// Nextest isolates this test from other tests allocating the closed number.
fn reuse_descriptor(fd: std::os::fd::RawFd) -> File {
    use std::os::fd::AsRawFd;

    let mut lower_descriptors = Vec::new();
    loop {
        let replacement = File::open("/dev/null").unwrap();
        assert!(
            replacement.as_raw_fd() <= fd,
            "CLOSE did not release descriptor {fd}"
        );
        if replacement.as_raw_fd() == fd {
            return replacement;
        }
        // open returns the lowest available number. Retain any lower slots so
        // the next open advances toward fd, without an arbitrary retry limit.
        lower_descriptors.push(replacement);
    }
}

#[test]
fn close_completion_never_closes_a_reused_descriptor() {
    use std::os::fd::AsRawFd;
    let mut driver = Driver::new(1).unwrap();
    let file = Fd::new(File::open("/dev/null").unwrap()).unwrap();
    let fd = file.as_raw_fd();
    let mut close = driver.push(Operation::close(file)).unwrap();
    driver.submit_and_wait(None).unwrap();
    // CLOSE ran, but its CQE has not been processed. Reuse its number before
    // reaping, so a second close would affect a new, independent owner.
    let replacement = reuse_descriptor(fd);
    finish(&mut driver, &mut close).result.unwrap();
    close.release();
    let status = replacement.metadata();
    if status.is_err() {
        // Report a double close without an IO-safety abort during unwinding.
        std::mem::forget(replacement);
    }
    status.unwrap();
    driver.drain().unwrap();
}

#[test]
fn ring_descriptor_import_does_not_leak() {
    use std::os::fd::AsRawFd;

    let ring = io_uring::IoUring::new(1).unwrap();
    let file = File::from(
        std::os::fd::AsFd::as_fd(&ring)
            .try_clone_to_owned()
            .unwrap(),
    );
    let fd = file.as_raw_fd();
    let path = format!("/proc/self/fd/{fd}");
    assert!(std::fs::read_link(&path).is_ok());

    // A File can contain a ring descriptor. The checked import must reject it
    // before it can reach CLOSE, and dispose of the transferred ownership.
    let error = Fd::try_from(file).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert_eq!(
        std::fs::read_link(&path).err().map(|error| error.kind()),
        Some(std::io::ErrorKind::NotFound),
        "rejected import leaked descriptor {fd}"
    );
    drop(ring);
}

#[test]
fn close_flush_error_never_closes_a_reused_descriptor() {
    use std::os::fd::AsRawFd;

    let mut driver = Driver::new(1).unwrap();
    // Unlike /dev/null, a regular file lets vwsim explore filesystem flush
    // errors, including ECANCELED after CLOSE has already removed the fd.
    // No cancellation is requested in this test.
    let file = Fd::new(tempfile::tempfile().unwrap()).unwrap();
    let fd = file.as_raw_fd();
    let mut close = driver.push(Operation::close(file)).unwrap();
    driver.submit_and_wait(None).unwrap();

    // Leave the CLOSE CQE unreaped while another owner acquires its fd number.
    let replacement = reuse_descriptor(fd);
    assert!(replacement.metadata().is_ok());

    let result = finish(&mut driver, &mut close).result;
    let close_errno = result.as_ref().err().and_then(std::io::Error::raw_os_error);
    let replacement_status = replacement.metadata();
    let replacement_errno = replacement_status
        .as_ref()
        .err()
        .and_then(std::io::Error::raw_os_error);
    if replacement_status.is_err() {
        // The bug has already closed this File's fd. Avoid a second close (or
        // an IO-safety abort during unwinding) obscuring the actual assertion.
        std::mem::forget(replacement);
    }
    assert!(
        replacement_status.is_ok(),
        "CLOSE errno {close_errno:?} closed the unrelated replacement fd {fd}; replacement errno {replacement_errno:?}"
    );
    driver.drain().unwrap();
}

#[test]
fn retained_results_grow_slab_without_moving_kernel_buffers() {
    let mut driver = Driver::new(1).unwrap();
    let file = std::rc::Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let mut handles = Vec::new();

    for offset in 0..64 {
        let buffer = Buffer::from_slice(b"stable", 64).unwrap();
        let pointer = buffer.as_ptr();
        let handle = driver
            .push(Operation::WriteAt {
                file: file.clone(),
                buffer,
                offset: offset * 6,
            })
            .unwrap();
        while driver.in_flight() != 0 {
            driver.submit_and_wait(None).unwrap();
            driver.tick();
        }
        handles.push((handle, pointer));
    }
    for (mut handle, pointer) in handles {
        let done = handle.try_complete().unwrap();
        assert_eq!(done.result.unwrap().into_count(), 6);
        assert_eq!(done.operation.buffer().unwrap().as_ptr(), pointer);
    }
    driver.drain().unwrap();
}

#[test]
fn zero_length_and_eof_preserve_initialized_buffers_and_file_position() {
    let mut driver = Driver::new(1).unwrap();
    let mut original = tempfile::tempfile().unwrap();
    original.set_len(16).unwrap();
    original.seek(SeekFrom::Start(7)).unwrap();
    let file = std::rc::Rc::new(Fd::new(original.try_clone().unwrap()).unwrap());
    for operation in [
        Operation::ReadAt {
            file: file.clone(),
            buffer: Buffer::new(0, 64).unwrap(),
            offset: 0,
        },
        Operation::WriteAt {
            file: file.clone(),
            buffer: Buffer::new(0, 64).unwrap(),
            offset: 0,
        },
        Operation::ReadAt {
            file,
            buffer: Buffer::new(32, 64).unwrap(),
            offset: 16,
        },
    ] {
        let mut handle = driver.push(operation).unwrap();
        let done = finish(&mut driver, &mut handle);
        assert_eq!(done.result.unwrap().into_count(), 0);
        assert!(done.operation.buffer().unwrap().iter().all(|b| *b == 0));
        assert_eq!(original.stream_position().unwrap(), 7);
    }
    driver.drain().unwrap();
}

#[test]
fn invalid_capacity_and_position_are_rejected() {
    assert!(Driver::new(0).is_err());
    assert!(Driver::new(32769).is_err());
    let driver = Driver::new(1).unwrap();
    let file = std::rc::Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let buffer = Buffer::from_slice(b"untouched", 8).unwrap();
    let pointer = buffer.as_ptr();
    let rejected = driver
        .push(Operation::ReadAt {
            file,
            buffer,
            offset: u64::MAX,
        })
        .unwrap_err();
    assert_eq!(rejected.error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(rejected.operation.buffer().unwrap().as_ptr(), pointer);
    assert_eq!(driver.in_flight(), 0);
}

fn descriptors_for(path: &std::path::Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| std::fs::read_link(entry.path()).is_ok_and(|target| target == path))
        .count()
}

#[test]
fn ignored_open_output_is_closed_when_its_cqe_is_reaped() {
    let original = tempfile::NamedTempFile::new().unwrap();
    let mut driver = Driver::new(1).unwrap();
    assert_eq!(descriptors_for(original.path()), 1);
    let path = CString::new(original.path().as_os_str().as_encoded_bytes()).unwrap();
    driver
        .push(Operation::OpenAt {
            directory: None,
            path,
            flags: libc::O_RDONLY,
            mode: 0,
        })
        .unwrap()
        .release();
    driver.submit_and_wait(None).unwrap();
    // The kernel-created descriptor exists before its ignored result is reaped.
    assert_eq!(descriptors_for(original.path()), 2);
    assert_eq!(driver.tick(), 1);
    assert_eq!(descriptors_for(original.path()), 1);
    driver.drain().unwrap();
}

#[test]
fn close_rejects_cancellation_and_finishes_during_teardown() {
    for submitted in [false, true] {
        for abandon in [false, true] {
            let original = tempfile::NamedTempFile::new().unwrap();
            let mut driver = Driver::new(1).unwrap();
            let io = driver.io().clone();
            let mut close = driver
                .push(Operation::close(
                    Fd::new(File::open(original.path()).unwrap()).unwrap(),
                ))
                .unwrap();
            if submitted {
                driver.submit().unwrap();
            }
            let rejected = driver
                .push(Operation::Cancel(close.cancel_target()))
                .unwrap_err();
            assert_eq!(rejected.error.kind(), std::io::ErrorKind::Unsupported);
            assert_eq!(driver.in_flight(), 1);
            // Dropping the returned cancellation token must release its pin.
            drop(rejected);
            if abandon {
                close.release();
            }
            drop(driver);
            assert_eq!(io.in_flight(), 0);
            if !abandon {
                let done = close.try_complete().expect("teardown reaped CLOSE");
                assert_close_result(done.result);
            }
            // Teardown must run CLOSE even if it was queued or abandoned.
            assert_eq!(descriptors_for(original.path()), 1);
        }
    }
}

#[test]
fn owning_handle_always_uses_its_origin_and_rejects_foreign_cancellation() {
    assert_eq!(size_of::<Op<Operation>>(), 88);
    let mut first = Driver::new(1).unwrap();
    let mut other = Driver::new(1).unwrap();
    let mut a = first.push(Operation::Nop).unwrap();
    let mut b = other.push(Operation::Nop).unwrap();
    // Both rings allocated their first slot. Only the owner can choose its arena.
    assert!(a.try_complete().is_none());
    let rejected = other.io().submit(a.cancel_target()).unwrap_err();
    assert_eq!(rejected.error.kind(), std::io::ErrorKind::InvalidInput);
    drop(rejected);
    finish(&mut other, &mut b).result.unwrap();
    assert!(a.try_complete().is_none());
    assert_eq!(first.in_flight(), 1);
    assert_eq!(other.in_flight(), 0);
    finish(&mut first, &mut a).result.unwrap();
    first.drain().unwrap();
    other.drain().unwrap();
}

#[test]
fn release_is_idempotent_and_keeps_kernel_resources_until_reaping() {
    let mut driver = Driver::new(1).unwrap();
    let file = std::rc::Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let weak = std::rc::Rc::downgrade(&file);
    let mut handle = driver
        .push(Operation::WriteAt {
            file,
            buffer: Buffer::new(8, 1).unwrap(),
            offset: 0,
        })
        .unwrap();
    handle.release();
    handle.release();
    assert!(weak.upgrade().is_some());
    driver.drain().unwrap();
    assert!(weak.upgrade().is_none());
}

#[test]
fn owners_retain_completed_inputs_after_driver_drop_without_an_external_io() {
    let mut driver = Driver::new(1).unwrap();
    let file = std::rc::Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let weak = std::rc::Rc::downgrade(&file);
    let mut handle = driver
        .push(Operation::ReadAt {
            file,
            buffer: Buffer::new(8, 1).unwrap(),
            offset: 0,
        })
        .unwrap();
    let _cancel = driver
        .push(Operation::Cancel(handle.cancel_target()))
        .unwrap();
    driver.drain().unwrap();
    // The owner retains its inputs and shared completion state after ring teardown.
    drop(driver);
    assert!(weak.upgrade().is_some());
    let mut driver = Driver::new(1).unwrap();
    let mut current = driver.push(Operation::Nop).unwrap();
    let completion = handle
        .try_complete()
        .expect("originating driver retired the read");
    assert!(weak.upgrade().is_some());
    assert!(current.try_complete().is_none());
    drop(completion);
    assert!(weak.upgrade().is_none());
    finish(&mut driver, &mut current).result.unwrap();
    driver.drain().unwrap();
    drop(handle);
    assert!(weak.upgrade().is_none());
}

#[test]
fn drop_cancels_queued_and_submitted_work_at_full_capacity() {
    use std::{
        os::unix::net::UnixStream,
        sync::atomic::{AtomicUsize, Ordering::Relaxed},
    };
    struct Count(AtomicUsize);
    impl std::task::Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Relaxed);
        }
    }
    let worker = std::thread::spawn(move || {
        for submitted in [false, true] {
            let (socket, peer) = UnixStream::pair().unwrap();
            let socket = std::rc::Rc::new(Fd::new(File::from(OwnedFd::from(socket))).unwrap());
            let weak = std::rc::Rc::downgrade(&socket);
            let mut driver = Driver::new(3).unwrap();
            let io = driver.io().clone();
            let mut receive = driver
                .push(Operation::Recv {
                    socket: socket.clone(),
                    buffer: Buffer::new(8, 1).unwrap(),
                })
                .unwrap();
            let mut ignored = driver
                .push(Operation::Recv {
                    socket: socket.clone(),
                    buffer: Buffer::new(8, 1).unwrap(),
                })
                .unwrap();
            let mut watch = driver
                .push(Operation::PollAdd {
                    file: socket.clone(),
                    events: libc::POLLIN as u32,
                })
                .unwrap();
            drop(socket);
            // Leave a public cancellation outstanding too. Teardown must still
            // cancel the other receive and watch, without data from the peer.
            let mut cancellation = driver
                .push(Operation::Cancel(receive.cancel_target()))
                .unwrap();
            let pin = ignored.cancel_target();
            ignored.release();
            let count = Arc::new(Count(AtomicUsize::new(0)));
            let waker = Waker::from(count.clone());
            let mut cx = Context::from_waker(&waker);
            assert!(receive.poll_complete(&mut cx).is_pending());
            assert!(watch.poll_complete(&mut cx).is_pending());
            if submitted {
                driver.submit().unwrap();
            }
            drop(driver);
            assert_eq!(io.in_flight(), 0);
            assert!(!io.has_pending());
            assert_eq!(count.0.load(Relaxed), 2);
            assert_eq!(
                io.push(Operation::Nop).unwrap_err().error.kind(),
                std::io::ErrorKind::BrokenPipe
            );
            for mut handle in [receive, watch] {
                let result = handle.try_complete().expect("drop reaped original CQE");
                assert_eq!(
                    result.result.unwrap_err().raw_os_error(),
                    Some(libc::ECANCELED)
                );
            }
            let result = cancellation
                .try_complete()
                .expect("public cancel retired")
                .result;
            assert!(
                result.is_ok()
                    || matches!(
                        result.unwrap_err().raw_os_error(),
                        Some(libc::ENOENT | libc::EALREADY)
                    )
            );
            // A pin retains identity only, not completed inputs.
            assert!(weak.upgrade().is_none());
            drop(pin);
            assert!(weak.upgrade().is_none());
            drop(peer);
        }
    });
    worker.join().unwrap();
}

#[test]
fn drop_submits_queued_file_io_and_reaps_close_without_retaining_resources() {
    let driver = Driver::new(2).unwrap();
    let io = driver.io().clone();
    let file = std::rc::Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let weak = std::rc::Rc::downgrade(&file);
    let mut write = driver
        .push(Operation::WriteAt {
            file,
            buffer: Buffer::from_slice(b"queued", 1).unwrap(),
            offset: 0,
        })
        .unwrap();
    let close_file = tempfile::NamedTempFile::new().unwrap();
    driver
        .push(Operation::close(
            Fd::new(close_file.reopen().unwrap()).unwrap(),
        ))
        .unwrap()
        .release();
    assert_eq!(descriptors_for(close_file.path()), 2);
    drop(driver);
    assert_eq!(io.in_flight(), 0);
    assert_eq!(descriptors_for(close_file.path()), 1);
    let completion = write.try_complete().expect("queued write retired");
    // Cancellation may lose to ordinary completion.
    match completion.result {
        Ok(Value::Count(n)) => assert_eq!(n, 6),
        Err(error) => assert_eq!(error.raw_os_error(), Some(libc::ECANCELED)),
        result => panic!("unexpected write result: {result:?}"),
    }
    drop(completion.operation);
    assert!(weak.upgrade().is_none());
}

#[test]
fn drop_notifies_waiters_after_all_results_retire_and_ring_is_closed() {
    use std::cell::RefCell;
    thread_local! { static CALLBACK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) }; }
    struct Reenter;
    impl std::task::Wake for Reenter {
        fn wake(self: Arc<Self>) {
            CALLBACK.with(|slot| slot.borrow_mut().take()).unwrap()();
        }
    }
    let driver = Driver::new(2).unwrap();
    let io = driver.io().clone();
    let mut first = driver.push(Operation::Nop).unwrap();
    let mut second = driver.push(Operation::Nop).unwrap();
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output = observed.clone();
    let callback_io = io.clone();
    CALLBACK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            assert_eq!(callback_io.in_flight(), 0);
            assert!(!callback_io.has_pending());
            assert!(second.try_complete().is_some());
            assert_eq!(
                callback_io.push(Operation::Nop).unwrap_err().error.kind(),
                std::io::ErrorKind::BrokenPipe
            );
            output.store(true, std::sync::atomic::Ordering::Relaxed);
        }))
    });
    let waker = Waker::from(Arc::new(Reenter));
    assert!(
        first
            .poll_complete(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(driver);
    assert!(observed.load(std::sync::atomic::Ordering::Relaxed));
    assert!(first.try_complete().is_some());
}

#[test]
fn send_range_validates_bounds_and_returns_original_buffer() {
    let mut driver = Driver::new(1).unwrap();
    let (sender, mut receiver) = std::os::unix::net::UnixStream::pair().unwrap();
    let sender = std::rc::Rc::new(Fd::new(sender).unwrap());
    let mut buffer = Buffer::from_slice(b"prefixPAYLOADsuffix", 64).unwrap();
    let ptr = buffer.as_ptr();
    for (start, end) in [(4, 3), (0, 100), (usize::MAX, usize::MAX)] {
        let rejected = driver
            .push(Operation::SendRange {
                socket: sender.clone(),
                buffer,
                range: start..end,
            })
            .unwrap_err();
        assert_eq!(rejected.error.kind(), std::io::ErrorKind::InvalidInput);
        buffer = rejected.operation.into_buffer().unwrap();
        assert_eq!(buffer.as_ptr(), ptr);
        assert_eq!(driver.io().in_flight(), 0);
    }
    let mut handle = driver
        .push(Operation::SendRange {
            socket: sender,
            buffer,
            range: 6..13,
        })
        .unwrap();
    let completion = finish(&mut driver, &mut handle);
    assert_eq!(completion.result.unwrap().into_count(), 7);
    let buffer = completion.operation.into_buffer().unwrap();
    assert_eq!(buffer.as_ptr(), ptr);
    assert_eq!(&*buffer, b"prefixPAYLOADsuffix");
    let mut bytes = [0; 7];
    receiver.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"PAYLOAD");
}

#[test]
fn outstanding_reads_grow_beyond_sq_capacity_and_cancel_across_turnovers() {
    let mut driver = Driver::new(2).unwrap();
    let io = driver.io().clone();
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let socket = std::rc::Rc::new(Fd::new(socket).unwrap());
    let mut reads = Vec::new();
    let mut pointers = Vec::new();
    for _ in 0..64 {
        let buffer = Buffer::new(8, 1).unwrap();
        pointers.push(buffer.as_ptr());
        reads.push(
            io.push(Operation::Recv {
                socket: socket.clone(),
                buffer,
            })
            .unwrap(),
        );
    }
    assert_eq!(io.in_flight(), 64, "SQ capacity does not cap blocked reads");
    let mut cancels: Vec<_> = reads
        .iter()
        .map(|read| io.push(Operation::Cancel(read.cancel_target())).unwrap())
        .collect();
    driver.drain().unwrap();
    for (read, pointer) in reads.iter_mut().zip(pointers) {
        let done = read.try_complete().unwrap();
        assert_eq!(
            done.result.unwrap_err().raw_os_error(),
            Some(libc::ECANCELED)
        );
        assert_eq!(done.operation.buffer().unwrap().as_ptr(), pointer);
    }
    for cancel in &mut cancels {
        cancel.try_complete().unwrap().result.unwrap();
    }
    assert_eq!(io.in_flight(), 0);
}

#[test]
fn repeated_sq_turnover_retains_all_unconsumed_completions() {
    let mut driver = Driver::new(2).unwrap();
    let mut handles: Vec<_> = (0..1024)
        .map(|_| driver.push(Operation::Nop).unwrap())
        .collect();
    driver.drain().unwrap();
    for handle in &mut handles {
        handle.try_complete().unwrap().result.unwrap();
    }
    assert_eq!(driver.in_flight(), 0);
}

#[test]
fn drop_retires_more_blocked_reads_than_ring_capacity_with_retained_pins() {
    let driver = Driver::new(2).unwrap();
    let io = driver.io().clone();
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let socket = std::rc::Rc::new(Fd::new(socket).unwrap());
    let weak = std::rc::Rc::downgrade(&socket);
    let mut handles: Vec<_> = (0..32)
        .map(|_| {
            io.push(Operation::Recv {
                socket: socket.clone(),
                buffer: Buffer::new(8, 1).unwrap(),
            })
            .unwrap()
        })
        .collect();
    let pin = handles[0].cancel_target();
    handles[0].release();
    drop(socket);
    assert_eq!(io.in_flight(), 32);
    drop(driver);
    assert_eq!(io.in_flight(), 0);
    for handle in &mut handles[1..] {
        assert_eq!(
            handle
                .try_complete()
                .unwrap()
                .result
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ECANCELED)
        );
    }
    assert!(
        weak.upgrade().is_none(),
        "unpublished cancellation pins identity but not retired resources"
    );
    drop(pin);
    assert!(weak.upgrade().is_none());
}

#[test]
fn completed_abandoned_open_closes_result_while_identity_remains_pinned() {
    let original = tempfile::NamedTempFile::new().unwrap();
    let mut driver = Driver::new(2).unwrap();
    let io = driver.io().clone();
    let op = io
        .push(Operation::OpenAt {
            directory: None,
            path: std::ffi::CString::new(original.path().as_os_str().as_encoded_bytes()).unwrap(),
            flags: libc::O_RDONLY,
            mode: 0,
        })
        .unwrap();
    let pin = op.cancel_target();
    while io.in_flight() != 0 {
        driver.submit_and_wait(None).unwrap();
        driver.tick();
    }
    assert_eq!(descriptors_for(original.path()), 2);
    drop(op);
    assert_eq!(
        descriptors_for(original.path()),
        1,
        "pin must not retain a returned fd"
    );
    // The identity still accepts its delayed cancellation after resource cleanup.
    let mut cancel = io.push(Operation::Cancel(pin)).unwrap();
    assert_eq!(
        finish(&mut driver, &mut cancel)
            .result
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOENT)
    );
}
