#![forbid(unsafe_code)]
use shep_smp_io2::{Driver, Operation, Progress};
use shep_smp_net::{Buffer, Config, TcpListener, TcpStream};
use shep_smp2::Runtime;
use std::{
    future::Future,
    io::{Read, Write},
    net::{Shutdown, SocketAddr},
    pin::pin,
    task::{Context, Waker},
    time::{Duration, Instant},
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
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    (client, server)
}
fn drain(driver: &mut Driver) {
    let end = Instant::now() + Duration::from_secs(3);
    let mut progress = Progress::default();
    while driver.io().in_flight() != 0 {
        assert!(Instant::now() < end, "socket work did not retire");
        driver.submit_and_wait(Some(end)).unwrap();
        driver.reap_into(&mut progress);
        progress.wake();
    }
}

#[test]
fn native_connect_accept_and_full_duplex_half_close() {
    let mut runtime = runtime();
    let io = runtime.io();
    let mut listener =
        TcpListener::bind(io.clone(), address(), Config { max_buffer_size: 7 }).unwrap();
    let address = listener.local_addr();
    runtime
        .block_on(async move {
            let server = shep_smp2::spawn(async move {
                let mut stream = listener.accept().await.unwrap();
                let (mut reader, mut writer) = stream.split();
                // Both directions remain independently usable; writes are chunked.
                writer.write_all(b"greeting").await.unwrap();
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
            let mut client = TcpStream::connect(io, address, Config::default())
                .await
                .unwrap();
            client
                .write_all(b"a message larger than the buffer limit")
                .await
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
fn bounds_and_saturation_return_original_buffer_without_closing_socket() {
    let mut driver = Driver::new(1).unwrap();
    let io = driver.io().clone();
    let (client, _peer) = pair();
    let mut stream =
        TcpStream::from_std(io.clone(), client, Config { max_buffer_size: 4 }).unwrap();
    let mut noop = io.push(Operation::Nop).unwrap();
    for (len, kind) in [
        (5, std::io::ErrorKind::InvalidInput),
        (4, std::io::ErrorKind::WouldBlock),
    ] {
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
    noop.try_complete(&io).unwrap().result.unwrap();
    assert!(
        pin!(stream.flush())
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
}

#[test]
fn dropping_multiple_queued_receives_retires_even_with_cancel_reserve_full() {
    let mut driver = Driver::new(2).unwrap();
    let io = driver.io().clone();
    let (a, mut peer_a) = pair();
    let (b, mut peer_b) = pair();
    let mut a = TcpStream::from_std(io.clone(), a, Config::default()).unwrap();
    let mut b = TcpStream::from_std(io.clone(), b, Config::default()).unwrap();
    let mut first = Box::pin(a.read(Buffer::new(8, 1).unwrap()));
    let mut second = Box::pin(b.read(Buffer::new(8, 1).unwrap()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert!(second.as_mut().poll(&mut cx).is_pending());
    assert_eq!(io.in_flight(), 2);
    drop(first); // Occupies the cancellation reserve.
    drop(second); // Shutdown must work even though Cancel is rejected.
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
        let mut a = TcpListener::bind(driver.io().clone(), address(), Config::default()).unwrap();
        let mut b = TcpListener::bind(driver.io().clone(), address(), Config::default()).unwrap();
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
    let mut listener = TcpListener::bind(io.clone(), address(), Config::default()).unwrap();
    let (stream, mut peer) = pair();
    let mut stream = TcpStream::from_std(io.clone(), stream, Config::default()).unwrap();
    runtime.spawn(async move { listener.accept().await });
    runtime.spawn(async move { stream.read(Buffer::new(8, 1).unwrap()).await });
    runtime
        .block_on(async { shep_smp2::sleep(Duration::from_millis(5)).await })
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
    let mut client = TcpStream::from_std(runtime.io(), client, Config::default()).unwrap();
    runtime
        .block_on(async move {
            let (result, _) = client.read(Buffer::new(8, 1).unwrap()).await;
            assert_eq!(result.unwrap(), 0);
            client.close().await.unwrap();
        })
        .unwrap();
    let (client, peer) = pair();
    rustix::net::sockopt::set_socket_linger(&peer, Some(Duration::ZERO)).unwrap();
    drop(peer);
    let mut client = TcpStream::from_std(runtime.io(), client, Config::default()).unwrap();
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
            let error = TcpStream::connect(io, address, Config::default())
                .await
                .err()
                .unwrap();
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
        })
        .unwrap();
    let (client, mut peer) = pair();
    let mut client = TcpStream::from_std(runtime.io(), client, Config::default()).unwrap();
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
    rustix::net::sockopt::set_socket_send_buffer_size(&client, 4096).unwrap();
    let length = 1024 * 1024;
    let bytes: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
    let expected = bytes.clone();
    let (sent_first, start_reading) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        start_reading.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut actual = Vec::new();
        peer.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, expected);
    });
    let mut runtime = runtime();
    let mut stream = TcpStream::from_std(
        runtime.io(),
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
            stream.write_all(&bytes[sent..]).await.unwrap();
            stream.close().await.unwrap();
        })
        .unwrap();
    reader.join().unwrap();
}

#[test]
fn dropping_blocked_sends_retires_without_cancellation_capacity() {
    let mut driver = Driver::new(2).unwrap();
    let mut streams = Vec::new();
    let mut peers = Vec::new();
    for _ in 0..2 {
        let (mut client, peer) = pair();
        rustix::net::sockopt::set_socket_send_buffer_size(&client, 4096).unwrap();
        client.set_nonblocking(true).unwrap();
        loop {
            match client.write(&[1; 65536]) {
                Ok(n) => assert_ne!(n, 0),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        streams.push(TcpStream::from_std(driver.io().clone(), client, Config::default()).unwrap());
        peers.push(peer);
    }
    let (first, second) = streams.split_at_mut(1);
    let mut a = Box::pin(first[0].write(Buffer::new(65536, 1).unwrap()));
    let mut b = Box::pin(second[0].write(Buffer::new(65536, 1).unwrap()));
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
fn dropping_connect_waits_at_full_capacity_retires_all_work() {
    let listener = std::net::TcpListener::bind(address()).unwrap();
    let address = listener.local_addr().unwrap();
    let mut driver = Driver::new(2).unwrap();
    let mut a = Box::pin(TcpStream::connect(
        driver.io().clone(),
        address,
        Config::default(),
    ));
    let mut b = Box::pin(TcpStream::connect(
        driver.io().clone(),
        address,
        Config::default(),
    ));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(a.as_mut().poll(&mut cx).is_pending());
    assert!(b.as_mut().poll(&mut cx).is_pending());
    drop(a);
    drop(b);
    drain(&mut driver);
}
