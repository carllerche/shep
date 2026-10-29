#![forbid(unsafe_code)]
use shep_smp2::Runtime;
use shep_smp2::io::{Driver, Operation};
use shep_smp2::net::{Buffer, Config, TcpListener, TcpStream};
use std::{
    future::Future,
    io::{Read, Write},
    net::{Shutdown, SocketAddr},
    pin::pin,
    task::{Context, Waker},
    time::Duration,
};

fn address() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}
fn runtime() -> Runtime {
    Runtime::new(shep_smp2::Config::default()).unwrap()
}
fn pair() -> (std::net::TcpStream, std::net::TcpStream) {
    let listener = std::net::TcpListener::bind(address()).unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}
fn drain(driver: &mut Driver) {
    while driver.io().in_flight() != 0 {
        driver.submit_and_wait(None).unwrap();
        driver.tick();
    }
}

#[test]
fn native_connect_accept_and_full_duplex_half_close() {
    let mut runtime = runtime();
    let io = runtime.io();
    let mut listener = TcpListener::bind(&io, address(), Config { max_buffer_size: 7 }).unwrap();
    let address = listener.local_addr();
    runtime
        .block_on(async move {
            let server = shep_smp2::spawn(async move {
                let mut stream = listener.accept().await.unwrap();
                let (mut reader, mut writer) = stream.split();
                // Both directions remain independently usable; writes are chunked.
                let buffer = Buffer::from_slice(b"greeting", 64).unwrap();
                let ptr = buffer.as_ptr();
                let (result, buffer) = writer.write_all(buffer).await;
                result.unwrap();
                assert_eq!(buffer.as_ptr(), ptr);
                assert_eq!(&*buffer, b"greeting");
                writer.shutdown().await.unwrap();
                let mut received = Vec::new();
                loop {
                    let (result, buffer) = reader.read(Buffer::new(7, 1).unwrap()).await;
                    let n = result.unwrap();
                    if n == 0 {
                        break;
                    }
                    received.extend_from_slice(&buffer[..n]);
                }
                assert_eq!(received, b"a message larger than the buffer limit");
            });
            let mut client = TcpStream::connect(&io, address, Config::default())
                .await
                .unwrap();
            client
                .write_all(
                    Buffer::from_slice(b"a message larger than the buffer limit", 1).unwrap(),
                )
                .await
                .0
                .unwrap();
            client.flush().await.unwrap();
            client.shutdown().await.unwrap();
            client.shutdown().await.unwrap();
            let (result, _) = client.write(Buffer::from_slice(b"late", 1).unwrap()).await;
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
            let mut received = Vec::new();
            loop {
                let (result, buffer) = client.read(Buffer::new(32, 1).unwrap()).await;
                let n = result.unwrap();
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&buffer[..n]);
            }
            assert_eq!(received, b"greeting");
            server.await.unwrap();
        })
        .unwrap();
    runtime.shutdown().unwrap();
}

#[test]
fn bounds_return_original_buffer_without_closing_socket() {
    let mut driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let (client, _peer) = pair();
    let mut stream = TcpStream::from_std(&io, client, Config { max_buffer_size: 4 }).unwrap();
    let mut noop = io.push(Operation::Nop).unwrap();
    for (len, kind) in [(5, std::io::ErrorKind::InvalidInput)] {
        let buffer = Buffer::new(len, 1).unwrap();
        let pointer = buffer.as_ptr();
        let mut future = pin!(stream.write(buffer));
        let (result, buffer) = match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            std::task::Poll::Ready(result) => result,
            _ => panic!("rejection should be immediate"),
        };
        assert_eq!(result.unwrap_err().kind(), kind);
        assert_eq!(pointer, buffer.as_ptr());
    }
    drain(&mut driver);
    noop.try_complete().unwrap().result.unwrap();
    assert!(
        pin!(stream.flush())
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
}

#[test]
fn dropping_multiple_receives_retires_across_sq_turnover() {
    let mut driver = Driver::new(2).unwrap();
    let io = driver.io().clone();
    let (a, mut peer_a) = pair();
    let (b, mut peer_b) = pair();
    let mut a = TcpStream::from_std(&io, a, Config::default()).unwrap();
    let mut b = TcpStream::from_std(&io, b, Config::default()).unwrap();
    let mut first = Box::pin(a.read(Buffer::new(8, 1).unwrap()));
    let mut second = Box::pin(b.read(Buffer::new(8, 1).unwrap()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert!(second.as_mut().poll(&mut cx).is_pending());
    assert_eq!(io.in_flight(), 2);
    drop(first); // Publish cancellation, turning over the SQ if necessary.
    drop(second); // Concurrent cancellation has no separate outstanding cap.
    drain(&mut driver);
    assert_eq!(peer_a.read(&mut [0; 1]).unwrap(), 0);
    assert_eq!(peer_b.read(&mut [0; 1]).unwrap(), 0);
    let mut future = pin!(a.read(Buffer::new(8, 1).unwrap()));
    let std::task::Poll::Ready((result, _)) = future.as_mut().poll(&mut cx) else {
        panic!()
    };
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotConnected);
}

#[test]
fn abandoned_accepts_retire_before_and_after_submission() {
    for submitted in [false, true] {
        let mut driver = Driver::new(2).unwrap();
        let mut a = TcpListener::bind(driver.io(), address(), Config::default()).unwrap();
        let mut b = TcpListener::bind(driver.io(), address(), Config::default()).unwrap();
        let mut first = Box::pin(a.accept());
        let mut second = Box::pin(b.accept());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        if submitted {
            driver.submit().unwrap();
        }
        drop(first);
        drop(second);
        drain(&mut driver);
        let std::task::Poll::Ready(result) = pin!(a.accept()).poll(&mut cx) else {
            panic!()
        };
        assert_eq!(
            result.err().unwrap().kind(),
            std::io::ErrorKind::NotConnected
        );
    }
}

#[test]
fn runtime_shutdown_aborts_live_accept_and_receive_tasks() {
    let mut runtime = runtime();
    let io = runtime.io();
    let mut listener = TcpListener::bind(&io, address(), Config::default()).unwrap();
    let (stream, mut peer) = pair();
    let mut stream = TcpStream::from_std(&io, stream, Config::default()).unwrap();
    let (accept_started, accept_ready) = tokio::sync::oneshot::channel();
    let (read_started, read_ready) = tokio::sync::oneshot::channel();
    runtime.spawn(async move {
        let mut accept = pin!(listener.accept());
        let mut started = Some(accept_started);
        std::future::poll_fn(|cx| {
            let result = accept.as_mut().poll(cx);
            if let Some(started) = started.take() {
                assert!(result.is_pending());
                started.send(()).unwrap();
            }
            result
        })
        .await
    });
    runtime.spawn(async move {
        let mut read = pin!(stream.read(Buffer::new(8, 1).unwrap()));
        let mut started = Some(read_started);
        std::future::poll_fn(|cx| {
            let result = read.as_mut().poll(cx);
            if let Some(started) = started.take() {
                assert!(result.is_pending());
                started.send(()).unwrap();
            }
            result
        })
        .await
    });
    runtime
        .block_on(async {
            accept_ready.await.unwrap();
            read_ready.await.unwrap();
        })
        .unwrap();
    runtime.shutdown().unwrap();
    assert_eq!(io.in_flight(), 0);
    assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
}

#[test]
fn peer_eof_and_reset_are_reported() {
    let mut runtime = runtime();
    let (client, peer) = pair();
    peer.shutdown(Shutdown::Write).unwrap();
    let mut client = TcpStream::from_std(&runtime.io(), client, Config::default()).unwrap();
    runtime
        .block_on(async move {
            let (result, _) = client.read(Buffer::new(8, 1).unwrap()).await;
            assert_eq!(result.unwrap(), 0);
            client.close().await.unwrap();
        })
        .unwrap();
    let (client, peer) = pair();
    shep_sys::set_socket_linger(&peer, Some(Duration::ZERO)).unwrap();
    drop(peer);
    let mut client = TcpStream::from_std(&runtime.io(), client, Config::default()).unwrap();
    runtime
        .block_on(async move {
            let (result, _) = client.read(Buffer::new(8, 1).unwrap()).await;
            assert_eq!(
                result.unwrap_err().kind(),
                std::io::ErrorKind::ConnectionReset
            );
        })
        .unwrap();
}

#[test]
fn connect_refusal_and_unpolled_futures() {
    let listener = std::net::TcpListener::bind(address()).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut runtime = runtime();
    let io = runtime.io();
    runtime
        .block_on(async move {
            let error = TcpStream::connect(&io, address, Config::default())
                .await
                .err()
                .unwrap();
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
        })
        .unwrap();
    let (client, mut peer) = pair();
    let mut client = TcpStream::from_std(&runtime.io(), client, Config::default()).unwrap();
    drop(client.read(Buffer::new(8, 1).unwrap()));
    drop(client.write(Buffer::from_slice(b"unsent", 1).unwrap()));
    peer.write_all(b"ok").unwrap();
    runtime
        .block_on(async move {
            let (result, buffer) = client.read(Buffer::new(8, 1).unwrap()).await;
            assert_eq!(&buffer[..result.unwrap()], b"ok");
        })
        .unwrap();
}

#[test]
fn short_writes_and_write_all_preserve_byte_order_under_backpressure() {
    let (client, mut peer) = pair();
    shep_sys::set_socket_send_buffer_size(&client, 4096).unwrap();
    let length = 1024 * 1024;
    let bytes: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
    let expected = bytes.clone();
    let (sent_first, start_reading) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        start_reading.recv().unwrap();
        let mut actual = Vec::new();
        peer.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, expected);
    });
    let mut runtime = runtime();
    let mut stream = TcpStream::from_std(
        &runtime.io(),
        client,
        Config {
            max_buffer_size: length,
        },
    )
    .unwrap();
    runtime
        .block_on(async move {
            let (result, buffer) = stream.write(Buffer::from_slice(&bytes, 1).unwrap()).await;
            let sent = result.unwrap();
            sent_first.send(()).unwrap();
            assert!(
                sent > 0 && sent < bytes.len(),
                "expected a short send, got {sent}"
            );
            assert_eq!(&*buffer, &bytes);
            let buffer = Buffer::from_slice(&bytes[sent..], 64).unwrap();
            let ptr = buffer.as_ptr();
            let (result, buffer) = stream.write_all(buffer).await;
            result.unwrap();
            assert_eq!(buffer.as_ptr(), ptr);
            assert_eq!(&*buffer, &bytes[sent..]);
            stream.close().await.unwrap();
        })
        .unwrap();
    reader.join().unwrap();
}

#[test]
fn dropping_blocked_sends_retires_under_sq_pressure() {
    let mut driver = Driver::new(2).unwrap();
    let mut streams = Vec::new();
    let mut peers = Vec::new();
    for _ in 0..2 {
        let (mut client, peer) = pair();
        shep_sys::set_socket_send_buffer_size(&client, 4096).unwrap();
        client.set_nonblocking(true).unwrap();
        loop {
            match client.write(&[1; 65536]) {
                Ok(n) => assert_ne!(n, 0),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        streams.push(TcpStream::from_std(driver.io(), client, Config::default()).unwrap());
        peers.push(peer);
    }
    let (first, second) = streams.split_at_mut(1);
    let mut a = Box::pin(first[0].write(Buffer::new(65536, 1).unwrap()));
    let mut b = Box::pin(second[0].write_all(Buffer::new(65536, 1).unwrap()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(a.as_mut().poll(&mut cx).is_pending());
    assert!(b.as_mut().poll(&mut cx).is_pending());
    driver.submit().unwrap();
    drop(a);
    drop(b);
    drain(&mut driver);
    drop(peers);
}

#[test]
fn dropping_connect_waits_retires_all_work_with_a_small_sq() {
    let listener = std::net::TcpListener::bind(address()).unwrap();
    let address = listener.local_addr().unwrap();
    let mut driver = Driver::new(2).unwrap();
    let mut a = Box::pin(TcpStream::connect(driver.io(), address, Config::default()));
    let mut b = Box::pin(TcpStream::connect(driver.io(), address, Config::default()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(a.as_mut().poll(&mut cx).is_pending());
    assert!(b.as_mut().poll(&mut cx).is_pending());
    drop(a);
    drop(b);
    drain(&mut driver);
}

#[test]
fn write_all_returns_owned_buffer_on_rejection_and_handles_empty_buffers() {
    let mut driver = Driver::new(1).unwrap();
    let (client, _peer) = pair();
    let mut stream =
        TcpStream::from_std(driver.io(), client, Config { max_buffer_size: 2 }).unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    let buffer = Buffer::new(0, 64).unwrap();
    let ptr = buffer.as_ptr();
    let mut empty = Box::pin(stream.write_all(buffer));
    let std::task::Poll::Ready((result, buffer)) = empty.as_mut().poll(&mut cx) else {
        panic!("empty write must complete immediately")
    };
    result.unwrap();
    assert_eq!(buffer.as_ptr(), ptr);
    drop(empty);
    let mut occupied = driver.push(Operation::Nop).unwrap();
    driver.io().close();
    let buffer = Buffer::from_slice(b"more than the send limit", 64).unwrap();
    let ptr = buffer.as_ptr();
    let mut write = Box::pin(stream.write_all(buffer));
    let std::task::Poll::Ready((result, buffer)) = write.as_mut().poll(&mut cx) else {
        panic!("admission rejection must complete immediately")
    };
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(buffer.as_ptr(), ptr);
    assert_eq!(&*buffer, b"more than the send limit");
    drop(write);
    occupied.release();
    drain(&mut driver);
}

#[test]
fn imported_lingering_socket_publishes_detached_close_under_sq_pressure() {
    use std::os::fd::AsRawFd;

    let (mut client, peer) = pair();
    shep_sys::set_socket_linger(&client, Some(Duration::from_secs(3))).unwrap();
    client.set_nonblocking(true).unwrap();
    let bytes = [0; 64 * 1024];
    let mut sent = 0;
    loop {
        match client.write(&bytes) {
            Ok(0) => panic!("socket stopped accepting data"),
            Ok(n) => sent += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("filling socket: {e}"),
        }
    }
    assert!(sent > 0);
    let raw = client.as_raw_fd();
    let mut runtime = Runtime::new(shep_smp2::Config {
        submission_queue_capacity: 2,
    })
    .unwrap();
    let io = runtime.io();
    // The watch plus this blocked receive fill the two-entry SQ. Drop must
    // submit them and publish CLOSE. Check the accepted request and descriptor
    // ownership directly; elapsed time cannot distinguish blocking from scheduling.
    let (blocked, blocked_peer) = pair();
    let mut blocked_read = io
        .push(Operation::Recv {
            socket: std::rc::Rc::new(shep_smp2::io::Fd::new(blocked).unwrap()),
            buffer: Buffer::new(1, 1).unwrap(),
        })
        .unwrap();
    let baseline = io.in_flight();
    let stream = TcpStream::from_std(&io, client, Config::default()).unwrap();
    let enter = runtime.enter();
    drop(stream);
    assert_eq!(
        io.in_flight(),
        baseline + 1,
        "Drop must publish a detached CLOSE"
    );
    assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_ok());
    runtime.block_on(async {}).unwrap();
    // Release the unread peer so the worker can finish promptly.
    drop(peer);
    drop(blocked_peer);
    blocked_read.release();
    runtime.shutdown().unwrap();
    drop(enter);
    assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_err());
}

#[test]
fn socket_drop_falls_back_when_close_cannot_be_published() {
    use std::os::fd::AsRawFd;

    for unavailable in 1..4 {
        let (client, _peer) = pair();
        let raw = client.as_raw_fd();
        let mut runtime = Some(
            Runtime::new(shep_smp2::Config {
                submission_queue_capacity: 1,
            })
            .unwrap(),
        );
        let io = runtime.as_ref().unwrap().io();
        let stream = TcpStream::from_std(&io, client, Config::default()).unwrap();
        let enter = (unavailable != 3).then(|| runtime.as_ref().unwrap().enter());
        match unavailable {
            1 => io.close(),
            2 => drop(runtime.take()),
            _ => {} // A live runtime exists, but is not entered.
        }
        drop(stream);
        assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_err());
        if let Some(runtime) = &mut runtime {
            runtime.shutdown().unwrap();
        }
        drop(enter);
        assert_eq!(io.in_flight(), 0);
    }
}
