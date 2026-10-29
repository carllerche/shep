#![cfg(not(miri))]

use shep_smp2::io::{Driver, Fd, Operation, Value};
use std::{fs::File, os::fd::AsRawFd};

fn poll<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    future.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
}

#[test]
fn explicit_async_close_waits_for_the_ring() {
    let mut driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let fd = Fd::new(File::open("/dev/null").unwrap()).unwrap();
    let raw = fd.as_raw_fd();
    let mut close = Box::pin(fd.close(&io));
    assert!(poll(close.as_mut()).is_pending());
    assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_ok());
    assert_eq!(driver.in_flight(), 1);
    loop {
        driver.submit_and_wait(None).unwrap();
        driver.tick();
        if let std::task::Poll::Ready(result) = poll(close.as_mut()) {
            result.unwrap();
            break;
        }
    }
    assert_eq!(driver.in_flight(), 0);
    assert_closed(raw);
}

#[test]
fn dropping_an_unpolled_close_uses_synchronous_fallback() {
    let driver = Driver::new(1).unwrap();
    let fd = Fd::new(File::open("/dev/null").unwrap()).unwrap();
    let raw = fd.as_raw_fd();
    drop(fd.close(driver.io()));
    assert_eq!(driver.in_flight(), 0);
    assert_closed(raw);
}

#[test]
fn dropping_a_published_close_leaves_cleanup_to_the_ring() {
    let driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let fd = Fd::new(File::open("/dev/null").unwrap()).unwrap();
    let raw = fd.as_raw_fd();
    let mut close = Box::pin(fd.close(&io));
    assert!(poll(close.as_mut()).is_pending());
    drop(close);
    assert_eq!(io.in_flight(), 1);
    assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_ok());
    drop(driver);
    assert_eq!(io.in_flight(), 0);
    assert_closed(raw);
}

#[test]
fn rejected_async_close_uses_synchronous_fallback() {
    let mut driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let mut nop = driver.push(Operation::Nop).unwrap();
    io.close();
    let fd = Fd::new(File::open("/dev/null").unwrap()).unwrap();
    let raw = fd.as_raw_fd();
    let mut close = Box::pin(fd.close(&io));
    let std::task::Poll::Ready(result) = poll(close.as_mut()) else {
        panic!("closed admission must reject immediately")
    };
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    assert_closed(raw);
    nop.release();
    driver.drain().unwrap();
}

#[test]
fn last_rc_owner_closes_synchronously_without_a_driver() {
    let fd = std::rc::Rc::new(Fd::new(File::open("/dev/null").unwrap()).unwrap());
    let raw = fd.as_raw_fd();
    let other = fd.clone();
    drop(fd);
    assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_ok());
    drop(other);
    assert_closed(raw);
}

fn assert_closed(fd: i32) {
    assert_eq!(
        std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .err()
            .map(|error| error.kind()),
        Some(std::io::ErrorKind::NotFound),
        "descriptor {fd} leaked"
    );
}

#[test]
fn a_standard_socket_wrapper_does_not_bypass_ring_validation() {
    let ring = io_uring::IoUring::new(1).unwrap();
    // This conversion is safe in std, but does not check the descriptor's type.
    let socket = std::net::TcpStream::from(
        std::os::fd::AsFd::as_fd(&ring)
            .try_clone_to_owned()
            .unwrap(),
    );
    let raw = socket.as_raw_fd();
    let error = Fd::try_from(socket).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert_closed(raw);
}

#[test]
fn anonymous_non_ring_descriptors_can_be_imported_and_closed() {
    for epoll in [false, true] {
        let owned = if epoll {
            shep_sys::epoll().unwrap()
        } else {
            shep_sys::eventfd(0, false).unwrap()
        };
        let fd = Fd::try_from(owned).unwrap();
        let raw = fd.as_raw_fd();
        let mut driver = Driver::new(1).unwrap();
        let mut close = driver.push(Operation::close(fd)).unwrap();
        driver.drain().unwrap();
        assert!(matches!(
            close.try_complete().unwrap().result,
            Ok(Value::Done)
        ));
        drop(driver);
        assert_closed(raw);
    }
}

#[test]
fn unchecked_import_and_clone_preserve_ownership() {
    let file = tempfile::tempfile().unwrap();
    // SAFETY: tempfile created a live filesystem descriptor, not an io_uring.
    let fd = unsafe { Fd::from_owned_fd_unchecked(file.into()) };
    let original = fd.as_raw_fd();
    let clone = fd.try_clone().unwrap();
    let duplicate = clone.as_raw_fd();
    assert_ne!(original, duplicate);
    drop(fd);
    assert_closed(original);
    clone.as_file().set_len(12).unwrap();
    let file = File::from(clone);
    assert_eq!(file.metadata().unwrap().len(), 12);
    drop(file);
    assert_closed(duplicate);
}

#[test]
fn kernel_created_descriptors_can_be_closed_without_reimporting() {
    let mut driver = Driver::new(1).unwrap();
    let mut socket = driver
        .push(Operation::Socket {
            domain: libc::AF_INET,
            socket_type: libc::SOCK_STREAM,
            protocol: 0,
        })
        .unwrap();
    driver.submit_and_wait(None).unwrap();
    driver.tick();
    let Value::File(fd) = socket.try_complete().unwrap().result.unwrap() else {
        panic!("socket did not return an Fd")
    };
    let raw = fd.as_raw_fd();
    let mut close = driver.push(Operation::close(fd)).unwrap();
    driver.drain().unwrap();
    close.try_complete().unwrap().result.unwrap();
    drop(driver);
    assert_closed(raw);
}
