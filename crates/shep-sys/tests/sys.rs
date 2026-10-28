use std::{
    fs::File,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

#[test]
fn connect_ipv4_and_ipv6() {
    for address in ["127.0.0.1:0", "[::1]:0"] {
        let listener = TcpListener::bind(address).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        let fd = shep_sys::tcp_socket(&address).unwrap();
        assert_ne!(shep_sys::file_flags(&fd).unwrap() & libc::O_NONBLOCK, 0);
        // SAFETY: F_GETFD borrows a live descriptor and takes no third argument.
        assert_ne!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        match shep_sys::connect(&fd, &address) {
            Ok(()) => {}
            Err(error) => assert_eq!(error.raw_os_error(), Some(libc::EINPROGRESS)),
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let (mut peer, _) = loop {
            match listener.accept() {
                Ok(pair) => break pair,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "connection timed out for {address}"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        };
        shep_sys::socket_error(&fd).unwrap();
        shep_sys::set_file_flags(&fd, 0).unwrap();
        let mut client = TcpStream::from(fd);
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        peer.write_all(b"hello").unwrap();
        let mut bytes = [0; 5];
        client.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"hello");
        shep_sys::shutdown(&client, std::net::Shutdown::Write).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        assert_eq!(peer.read(&mut bytes).unwrap(), 0);
    }
}

#[test]
fn event_counter_flags_and_drain() {
    let fd = shep_sys::eventfd(2, true).unwrap();
    assert_ne!(shep_sys::file_flags(&fd).unwrap() & libc::O_NONBLOCK, 0);
    let mut file = File::from(fd);
    file.write_all(&3u64.to_ne_bytes()).unwrap();
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes).unwrap();
    assert_eq!(u64::from_ne_bytes(bytes), 5);
    assert_eq!(
        file.read(&mut bytes).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn socket_options_validate_ranges_and_preserve_os_errors() {
    let fd = shep_sys::tcp_socket(&"127.0.0.1:0".parse().unwrap()).unwrap();
    shep_sys::set_socket_linger(&fd, Some(Duration::ZERO)).unwrap();
    shep_sys::set_socket_linger(&fd, None).unwrap();
    shep_sys::set_socket_send_buffer_size(&fd, 4096).unwrap();
    assert_eq!(
        shep_sys::set_socket_send_buffer_size(&fd, usize::MAX)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        shep_sys::set_socket_linger(&fd, Some(Duration::MAX))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    let file = File::open("/dev/null").unwrap();
    assert_eq!(
        shep_sys::socket_error(&file).unwrap_err().raw_os_error(),
        Some(libc::ENOTSOCK)
    );
}
