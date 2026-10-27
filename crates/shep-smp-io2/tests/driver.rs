#![forbid(unsafe_code)]
#![cfg(not(miri))]
use shep_smp_io2::{Buffer, Completion, Driver, Fd, Operation, OperationHandle, Progress, Value};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    net::{SocketAddr, TcpListener},
    os::fd::OwnedFd,
    sync::Arc,
    task::{Context, Waker},
};

fn finish(driver: &mut Driver, handle: &mut OperationHandle) -> Completion {
    let mut progress = Progress::default();
    loop {
        driver.reap_into(&mut progress);
        progress.wake();
        if let Some(done) = handle.try_complete(driver.io()) {
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
fn progress_accumulates_across_reaps_and_can_be_reused_after_waking() {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    struct Count(AtomicUsize);
    impl std::task::Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Relaxed);
        }
    }
    let mut driver = Driver::new(1).unwrap();
    let mut progress = Progress::default();
    let count = Arc::new(Count(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    for round in 0..3 {
        let mut handle = driver.push(Operation::Nop).unwrap();
        assert!(
            handle
                .poll_complete(driver.io(), &mut Context::from_waker(&waker))
                .is_pending()
        );
        let mut completed = 0;
        while driver.in_flight() != 0 {
            driver.submit_and_wait(None).unwrap();
            completed += driver.reap_into(&mut progress);
        }
        assert_eq!(completed, 1);
        assert_eq!(driver.reap_into(&mut progress), 0);
        handle.try_complete(driver.io()).unwrap().result.unwrap();
        assert_eq!(count.0.load(Relaxed), if round == 2 { 2 } else { 0 });
        if round >= 1 {
            progress.wake();
            assert_eq!(count.0.load(Relaxed), round + 1);
        }
    }
    progress.wake();
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
    assert_eq!(driver.reap_into(&mut Progress::default()), 0);
    assert!(
        handles
            .iter_mut()
            .all(|h| h.try_complete(driver.io()).is_none())
    );
    assert_eq!(driver.submit().unwrap(), 8);
    assert!(!driver.has_pending());
    for handle in &mut handles {
        finish(&mut driver, handle).result.unwrap();
    }
    driver.drain().unwrap();
}

#[test]
fn full_admission_rejects_immediately_with_original_buffer() {
    let mut driver = Driver::new(1).unwrap();
    let mut first = driver.push(Operation::Nop).unwrap();
    let buffer = Buffer::from_slice(b"ownership", 64).unwrap();
    let address = buffer.as_ptr();
    let rejected = driver
        .push(Operation::WriteAt {
            file: Arc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap()),
            buffer,
            offset: 0,
        })
        .unwrap_err();
    assert_eq!(rejected.error.kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(rejected.operation.buffer().unwrap().as_ptr(), address);
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
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

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
    let file = Arc::new(Fd::new(Arc::try_unwrap(file).unwrap()).unwrap());
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
        let flags = fcntl_getfl(&alias).unwrap();
        assert!(!flags.contains(OFlags::APPEND));
        fcntl_setfl(&alias, flags | OFlags::APPEND).unwrap();
    }
    let flags = fcntl_getfl(&alias).unwrap();
    assert!(flags.contains(OFlags::APPEND));
    let done = finish(&mut driver, &mut handle);
    assert_eq!(done.result.unwrap().into_count(), 1);
    assert_eq!(done.operation.buffer().unwrap().as_ptr(), ptr);
    assert_eq!(std::fs::read(temp.path()).unwrap(), b"Xbc");
    assert_eq!(alias.stream_position().unwrap(), 0);
    assert_eq!(fcntl_getfl(&alias).unwrap(), flags);
    // The override is per request; ordinary writes through the clone still append.
    alias.write_all(b"Y").unwrap();
    assert_eq!(std::fs::read(temp.path()).unwrap(), b"XbcY");
    driver.drain().unwrap();
}

#[test]
fn positional_owned_buffers_and_errors_preserve_allocation() {
    let mut driver = Driver::new(2).unwrap();
    let mut original = tempfile::tempfile().unwrap();
    let file = Arc::new(Fd::new(original.try_clone().unwrap()).unwrap());
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
            file: Arc::new(Fd::new(File::open("/dev/null").unwrap()).unwrap()),
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
fn cancellation_reserve_works_at_capacity_and_retains_target_identity() {
    let mut driver = Driver::new(1).unwrap();
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let socket = Arc::new(Fd::new(File::from(OwnedFd::from(socket))).unwrap());
    let mut target = driver
        .push(Operation::Recv {
            socket: socket.clone(),
            buffer: Buffer::new(8, 1).unwrap(),
        })
        .unwrap();
    let mut cancel = driver
        .push(Operation::Cancel(target.cancel_target(driver.io())))
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
    let token = first.cancel_target(driver.io());
    finish(&mut driver, &mut first).result.unwrap();
    let mut later = driver
        .push(Operation::Recv {
            socket,
            buffer: Buffer::new(8, 1).unwrap(),
        })
        .unwrap();
    let mut stale_cancel = driver.push(Operation::Cancel(token)).unwrap();
    assert!(finish(&mut driver, &mut stale_cancel).result.is_err());
    assert!(later.try_complete(driver.io()).is_none());
    let mut cancel = driver
        .push(Operation::Cancel(later.cancel_target(driver.io())))
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
        .push(Operation::Cancel(op.cancel_target(driver.io())))
        .unwrap_err();
    assert_eq!(rejected.error.kind(), std::io::ErrorKind::InvalidInput);
    drop(rejected);
    finish(&mut driver, &mut op).result.unwrap();
    driver.drain().unwrap();
}

#[test]
fn ignored_operations_keep_descriptors_until_cqe_and_results_survive_driver() {
    let mut driver = Driver::new(1).unwrap();
    let file = Arc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let weak = Arc::downgrade(&file);
    driver
        .push(Operation::WriteAt {
            file,
            buffer: Buffer::new(1, 1).unwrap(),
            offset: 0,
        })
        .unwrap()
        .release(driver.io());
    assert!(weak.upgrade().is_some());
    driver.drain().unwrap();
    assert!(weak.upgrade().is_none());
    let mut driver = Driver::new(1).unwrap();
    let mut op = driver.push(Operation::Nop).unwrap();
    driver.drain().unwrap();
    let io = driver.io().clone();
    drop(driver);
    assert!(op.try_complete(&io).unwrap().result.is_ok());
}

#[test]
fn open_allocate_truncate_sync_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut driver = Driver::new(1).unwrap();
    let mut open = driver
        .push(Operation::OpenAt {
            directory: Some(Arc::new(Fd::new(File::open(dir.path()).unwrap()).unwrap())),
            path: CString::new("created").unwrap(),
            flags: libc::O_CREAT | libc::O_RDWR,
            mode: 0o600,
        })
        .unwrap();
    let Value::File(file) = finish(&mut driver, &mut open).result.unwrap() else {
        panic!("fd result")
    };
    let file = Arc::new(file);
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
        .push(Operation::close(Arc::try_unwrap(file).unwrap()))
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
    let client = Arc::new(client);
    let listener = Arc::new(Fd::new(File::from(OwnedFd::from(listener))).unwrap());
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
    let server = Arc::new(server);
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
            .poll_complete(driver.io(), &mut Context::from_waker(&bad))
            .is_pending()
    );
    assert!(
        second
            .poll_complete(driver.io(), &mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    driver.drain().unwrap();
    assert!(first.try_complete(driver.io()).unwrap().result.is_ok());
    assert!(second.try_complete(driver.io()).unwrap().result.is_ok());
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
    let mut a = driver.push(Operation::Nop).unwrap();
    let mut b = driver.push(Operation::Nop).unwrap();
    let next = Rc::new(RefCell::new(None));
    let output = next.clone();
    let io = driver.io().clone();
    CALLBACK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            b.try_complete(&io)
                .expect("B retired before callback")
                .result
                .unwrap();
            *output.borrow_mut() = Some(io.push(Operation::Nop).unwrap());
        }))
    });
    let waker = Waker::from(Arc::new(Reenter));
    assert!(
        a.poll_complete(driver.io(), &mut Context::from_waker(&waker))
            .is_pending()
    );
    // Defer notification until the whole batch is retired, as a runtime may do.
    let mut progress = Progress::default();
    while driver.in_flight() != 0 {
        driver.submit_and_wait(None).unwrap();
        driver.reap_into(&mut progress);
    }
    progress.wake();
    a.try_complete(driver.io()).unwrap().result.unwrap();
    finish(&mut driver, next.borrow_mut().as_mut().unwrap())
        .result
        .unwrap();
    driver.drain().unwrap();
}

#[test]
fn close_completion_never_closes_a_reused_descriptor() {
    use std::os::fd::AsRawFd;
    let mut driver = Driver::new(1).unwrap();
    let file = Fd::new(File::open("/dev/null").unwrap()).unwrap();
    let fd = file.as_raw_fd();
    let mut close = driver.push(Operation::close(file)).unwrap();
    driver.submit_and_wait(None).unwrap();
    // CLOSE ran, but its CQE has not been processed. Keep replacements open
    // until the kernel allocates the old number to a new, independent owner.
    let mut replacements = Vec::new();
    for _ in 0..64 {
        let replacement = File::open("/dev/null").unwrap();
        let reused = replacement.as_raw_fd() == fd;
        replacements.push(replacement);
        if reused {
            break;
        }
    }
    finish(&mut driver, &mut close).result.unwrap();
    close.release(driver.io());
    replacements
        .iter()
        .find(|f| f.as_raw_fd() == fd)
        .unwrap_or_else(|| panic!("could not reuse fd"))
        .metadata()
        .unwrap();
    driver.drain().unwrap();
}

#[test]
fn ring_descriptor_import_does_not_leak() {
    use std::os::fd::AsRawFd;

    let ring = io_uring::IoUring::new(1).unwrap();
    let file = File::from(rustix::io::dup(&ring).unwrap());
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
    let mut replacements = Vec::new();
    for _ in 0..64 {
        let replacement = File::open("/dev/null").unwrap();
        let reused = replacement.as_raw_fd() == fd;
        replacements.push(replacement);
        if reused {
            break;
        }
    }
    let index = replacements
        .iter()
        .position(|file| file.as_raw_fd() == fd)
        .expect("could not reuse fd before reaping CLOSE");
    let replacement = replacements.swap_remove(index);
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
    let file = Arc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let mut handles = Vec::new();
    let mut progress = Progress::default();
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
            driver.reap_into(&mut progress);
            progress.wake();
        }
        handles.push((handle, pointer));
    }
    for (mut handle, pointer) in handles {
        let done = handle.try_complete(driver.io()).unwrap();
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
    let file = Arc::new(Fd::new(original.try_clone().unwrap()).unwrap());
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
    assert!(Driver::new(32768).is_err());
    let driver = Driver::new(1).unwrap();
    let file = Arc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
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
        .release(driver.io());
    driver.submit_and_wait(None).unwrap();
    // The kernel-created descriptor exists before its ignored result is reaped.
    assert_eq!(descriptors_for(original.path()), 2);
    assert_eq!(driver.reap_into(&mut Progress::default()), 1);
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
                .push(Operation::Cancel(close.cancel_target(&io)))
                .unwrap_err();
            assert_eq!(rejected.error.kind(), std::io::ErrorKind::Unsupported);
            assert_eq!(driver.in_flight(), 1);
            // Dropping the returned cancellation token must release its pin.
            drop(rejected);
            if abandon {
                close.release(&io);
            }
            drop(driver);
            assert_eq!(io.in_flight(), 0);
            if !abandon {
                let done = close.try_complete(&io).expect("teardown reaped CLOSE");
                assert_close_result(done.result);
            }
            // Teardown must run CLOSE even if it was queued or abandoned.
            assert_eq!(descriptors_for(original.path()), 1);
        }
    }
}

#[test]
fn handle_is_one_word_and_wrong_io_never_accesses_a_matching_slab_slot() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    assert_eq!(size_of::<OperationHandle>(), size_of::<u64>());
    let mut first = Driver::new(1).unwrap();
    let mut other = Driver::new(1).unwrap();
    let mut a = first.push(Operation::Nop).unwrap();
    let mut b = other.push(Operation::Nop).unwrap();
    // Both rings allocated their first slot; a clone of the same Io is valid.
    let io = first.io().clone();
    assert!(a.try_complete(&io).is_none());
    assert!(catch_unwind(AssertUnwindSafe(|| a.try_complete(other.io()))).is_err());
    assert!(
        catch_unwind(AssertUnwindSafe(
            || a.poll_complete(other.io(), &mut Context::from_waker(Waker::noop()))
        ))
        .is_err()
    );
    assert!(catch_unwind(AssertUnwindSafe(|| a.cancel_target(other.io()))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| a.release(other.io()))).is_err());
    assert_eq!(first.in_flight(), 1);
    assert_eq!(other.in_flight(), 1);
    finish(&mut first, &mut a).result.unwrap();
    finish(&mut other, &mut b).result.unwrap();
    first.drain().unwrap();
    other.drain().unwrap();
}

#[test]
fn release_is_idempotent_and_keeps_kernel_resources_until_reaping() {
    let mut driver = Driver::new(1).unwrap();
    let file = Arc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let weak = Arc::downgrade(&file);
    let mut handle = driver
        .push(Operation::WriteAt {
            file,
            buffer: Buffer::new(8, 1).unwrap(),
            offset: 0,
        })
        .unwrap();
    handle.release(driver.io());
    handle.release(driver.io());
    assert!(weak.upgrade().is_some());
    driver.drain().unwrap();
    assert!(weak.upgrade().is_none());
}

#[test]
fn handles_do_not_keep_the_driver_alive_and_new_drivers_reject_old_ids() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let mut driver = Driver::new(1).unwrap();
    let file = Arc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let weak = Arc::downgrade(&file);
    let mut handle = driver
        .push(Operation::ReadAt {
            file,
            buffer: Buffer::new(8, 1).unwrap(),
            offset: 0,
        })
        .unwrap();
    let _cancel = driver
        .push(Operation::Cancel(handle.cancel_target(driver.io())))
        .unwrap();
    driver.drain().unwrap();
    // A forgotten submitter claim retains its result until Io destruction;
    // the handle itself owns no Io reference and cannot keep that result alive.
    drop(driver);
    assert!(weak.upgrade().is_none());
    let mut driver = Driver::new(1).unwrap();
    let mut current = driver.push(Operation::Nop).unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| handle.try_complete(driver.io()))).is_err());
    finish(&mut driver, &mut current).result.unwrap();
    driver.drain().unwrap();
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
            let socket = Arc::new(Fd::new(File::from(OwnedFd::from(socket))).unwrap());
            let weak = Arc::downgrade(&socket);
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
            // Occupy the public cancellation reserve too. Teardown must still
            // cancel the other receive and watch, without data from the peer.
            let mut cancellation = driver
                .push(Operation::Cancel(receive.cancel_target(&io)))
                .unwrap();
            let pin = ignored.cancel_target(&io);
            ignored.release(&io);
            let count = Arc::new(Count(AtomicUsize::new(0)));
            let waker = Waker::from(count.clone());
            let mut cx = Context::from_waker(&waker);
            assert!(receive.poll_complete(&io, &mut cx).is_pending());
            assert!(watch.poll_complete(&io, &mut cx).is_pending());
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
                let result = handle.try_complete(&io).expect("drop reaped original CQE");
                assert_eq!(
                    result.result.unwrap_err().raw_os_error(),
                    Some(libc::ECANCELED)
                );
            }
            let result = cancellation
                .try_complete(&io)
                .expect("public cancel retired")
                .result;
            assert!(
                result.is_ok()
                    || matches!(
                        result.unwrap_err().raw_os_error(),
                        Some(libc::ENOENT | libc::EALREADY)
                    )
            );
            // An external cancellation identity still pins the ignored record;
            // releasing it after Driver destruction must reclaim its resources.
            assert!(weak.upgrade().is_some());
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
    let file = Arc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let weak = Arc::downgrade(&file);
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
        .release(&io);
    assert_eq!(descriptors_for(close_file.path()), 2);
    drop(driver);
    assert_eq!(io.in_flight(), 0);
    assert_eq!(descriptors_for(close_file.path()), 1);
    let completion = write.try_complete(&io).expect("queued write retired");
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
            assert!(second.try_complete(&callback_io).is_some());
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
            .poll_complete(&io, &mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(driver);
    assert!(observed.load(std::sync::atomic::Ordering::Relaxed));
    assert!(first.try_complete(&io).is_some());
}

#[test]
fn completion_guards_outlive_claims_and_release_outside_borrows() {
    use std::{cell::Cell, rc::Rc};
    struct Guard {
        io: shep_smp_io2::Io,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            // Reenter the arena; no kernel/arena borrow may span this callback.
            let _ = self.io.in_flight();
            self.drops.set(self.drops.get() + 1);
        }
    }
    let mut driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let drops = Rc::new(Cell::new(0));
    let guard = || Guard {
        io: io.clone(),
        drops: drops.clone(),
    };
    let mut handle = io.push_with_guard(Operation::Nop, guard()).unwrap();
    assert!(io.push_with_guard(Operation::Nop, guard()).is_err());
    assert_eq!(drops.get(), 1, "rejection releases guard immediately");
    handle.release(&io);
    assert_eq!(drops.get(), 1, "abandonment must retain guard");
    let mut progress = shep_smp_io2::Progress::default();
    // A successful wait attempt is not proof of completion. Reap until the
    // request retires, keeping guard release deferred until progress.wake().
    while io.in_flight() != 0 {
        driver.submit_and_wait(None).unwrap();
        driver.reap_into(&mut progress);
    }
    assert_eq!(io.in_flight(), 0);
    assert_eq!(drops.get(), 1, "notifications remain deferred");
    progress.wake();
    assert_eq!(drops.get(), 2);
    let mut pending = io.push_with_guard(Operation::Nop, guard()).unwrap();
    pending.release(&io);
    drop(driver);
    assert_eq!(drops.get(), 3, "driver teardown releases guards");
}
