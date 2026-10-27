#![cfg(not(miri))]

use shep_smp_io2::{Driver, Fd, Operation, Progress, Value};
use std::{fs::File, os::fd::AsRawFd};

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
    let socket = std::net::TcpStream::from(rustix::io::dup(&ring).unwrap());
    let raw = socket.as_raw_fd();
    let error = Fd::try_from(socket).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert_closed(raw);
}

#[test]
fn anonymous_non_ring_descriptors_can_be_imported_and_closed() {
    for epoll in [false, true] {
        let owned = if epoll {
            rustix::event::epoll::create(rustix::event::epoll::CreateFlags::CLOEXEC).unwrap()
        } else {
            rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC).unwrap()
        };
        let fd = Fd::try_from(owned).unwrap();
        let raw = fd.as_raw_fd();
        let mut driver = Driver::new(1).unwrap();
        let mut close = driver.push(Operation::close(fd)).unwrap();
        driver.drain().unwrap();
        assert!(matches!(
            close.try_complete(driver.io()).unwrap().result,
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
    driver.reap_into(&mut Progress::default());
    let Value::File(fd) = socket.try_complete(driver.io()).unwrap().result.unwrap() else {
        panic!("socket did not return an Fd")
    };
    let raw = fd.as_raw_fd();
    let mut close = driver.push(Operation::close(fd)).unwrap();
    driver.drain().unwrap();
    close.try_complete(driver.io()).unwrap().result.unwrap();
    drop(driver);
    assert_closed(raw);
}
