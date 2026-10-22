//! The server's side: corruption, the memory budget, serving handed-off
//! connections, and shutdown.

mod support;

use std::time::Duration;

use bytes::Bytes;
use shep_rpc::wire::{COMPRESSION_ZSTD, Header};
use shep_rpc::{RpcError, Server, ServerConfig, Transport};
use support::*;
use tokio::net::{TcpListener, TcpStream};

async fn wait_started(state: &State, n: usize) {
    while state.started() < n {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// One good echo over a raw socket, to show the connection was healthy.
async fn raw_echo(stream: &mut TcpStream, id: u32) {
    write_frame(stream, &Header::new(ECHO, id, b"ok"), b"ok").await;
    let (header, body) = read_frame(stream).await.unwrap();
    assert_eq!((header.correlation_id, header.meta), (id, 200));
    assert_eq!(body, "ok");
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (corrupted_header_at_client_test, from the server's side): a bad header
/// checksum closes the connection; new connections are fine.
#[tokio::test]
async fn corrupted_header_closes_connection() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    raw_echo(&mut stream, 1).await;

    let mut header = Header::new(ECHO, 2, b"testing...");
    header.correlation_id = 3; // not what the checksum covered
    write_frame(&mut stream, &header, b"testing...").await;
    assert_closed(&mut stream).await;
    assert_eq!(state.started(), 1);

    let t = Transport::connect(addr).await.unwrap();
    for _ in 0..10 {
        assert_eq!(
            t.call(ECHO, "testing...".into(), LONG).await.unwrap(),
            "testing..."
        );
    }
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (corrupted_data_at_server): a body that fails its checksum closes the
/// connection before the handler sees it.
#[tokio::test]
async fn corrupted_payload_closes_connection() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    raw_echo(&mut stream, 1).await;

    let header = Header::new(ECHO, 2, b"testing...");
    write_frame(&mut stream, &header, b"testing!!!").await;
    assert_closed(&mut stream).await;
    assert_eq!(state.started(), 1);

    let t = Transport::connect(addr).await.unwrap();
    for _ in 0..10 {
        assert_eq!(
            t.call(ECHO, "testing...".into(), LONG).await.unwrap(),
            "testing..."
        );
    }
}

/// Compression is reserved but not implemented: such a frame closes the
/// connection.
#[tokio::test]
async fn compressed_request_closes_connection() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut header = Header::new(ECHO, 1, b"zstd?");
    header.compression = COMPRESSION_ZSTD;
    header.header_checksum = header.compute_checksum();
    write_frame(&mut stream, &header, b"zstd?").await;
    assert_closed(&mut stream).await;
    assert_eq!(state.started(), 0);
}

/// A request that could never fit the memory budget closes the connection
/// instead of waiting forever.
#[tokio::test]
async fn request_over_memory_budget_closes_connection() {
    let config = ServerConfig { max_memory: 1024 };
    let (_server, addr, state) = start(config).await;

    // Only the header is needed; the server gives up before the body.
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let body = vec![7; 1025];
    write_frame(&mut stream, &Header::new(ECHO, 1, &body), &[]).await;
    assert_closed(&mut stream).await;

    let t = Transport::connect(addr).await.unwrap();
    let reply = t.call(ECHO, body.into(), LONG).await;
    assert!(matches!(reply, Err(RpcError::Disconnected)), "{reply:?}");

    // Exactly the budget is fine.
    let t = Transport::connect(addr).await.unwrap();
    let body = Bytes::from(vec![7; 1024]);
    assert_eq!(t.call(ECHO, body.clone(), LONG).await.unwrap(), body);
    assert_eq!(state.started(), 1);
}

/// Request bodies hold memory until their responses are written, across
/// all connections; a request that does not fit waits.
#[tokio::test]
async fn memory_budget_holds_back_requests() {
    let config = ServerConfig { max_memory: 100 };
    let (_server, addr, state) = start(config).await;
    let a = Transport::connect(addr).await.unwrap();
    let b = Transport::connect(addr).await.unwrap();

    let first = a.call(BLOCK, vec![1; 60].into(), LONG);
    wait_started(&state, 1).await;
    let second = b.call(BLOCK, vec![2; 60].into(), LONG);
    // 60 + 60 > 100: the second body is not read while the first is held.
    let small = b.call(ECHO, vec![3; 10].into(), LONG);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(state.started(), 1);

    state.release.add_permits(1);
    assert_eq!(first.await.unwrap(), vec![1; 60]);
    wait_started(&state, 2).await;
    state.release.add_permits(1);
    assert_eq!(second.await.unwrap(), vec![2; 60]);
    assert_eq!(small.await.unwrap(), vec![3; 10]);
}

/// A connection accepted elsewhere, here on another thread with its own
/// runtime, is served with `serve_connection`.
#[test]
fn serve_handed_off_connection() {
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    let state = std::sync::Arc::new(State::default());
    let server = Server::new(ServerConfig::default(), handler(state.clone()));
    let (handoff, receive) = std::sync::mpsc::channel::<std::net::TcpStream>();

    // The accepting core.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = std::thread::spawn(move || {
        let rt = runtime();
        let (stream, _) = rt.block_on(async {
            let listener = TcpListener::from_std(listener).unwrap();
            listener.accept().await.unwrap()
        });
        handoff.send(stream.into_std().unwrap()).unwrap();
    });
    // The serving core.
    let worker = std::thread::spawn({
        let server = server.clone();
        move || {
            let stream = receive.recv().unwrap();
            runtime().block_on(async {
                server
                    .serve_connection(TcpStream::from_std(stream).unwrap())
                    .await;
            });
        }
    });

    runtime().block_on(async {
        let t = Transport::connect(addr).await.unwrap();
        let calls: Vec<_> = (0..100u32)
            .map(|i| t.call(ECHO, Bytes::from(i.to_string()), LONG))
            .collect();
        for (i, reply) in futures::future::join_all(calls)
            .await
            .into_iter()
            .enumerate()
        {
            assert_eq!(reply.unwrap(), i.to_string());
        }
    });
    // The transport is gone, so the connection closes and the worker ends.
    acceptor.join().unwrap();
    worker.join().unwrap();
    assert_eq!(state.started(), 100);
}

/// Shutdown stops new work, lets in-flight handlers finish and their
/// responses go out, then closes connections.
#[tokio::test]
async fn graceful_shutdown() {
    let (server, addr, state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let in_flight = t.call(BLOCK, "in flight".into(), LONG);
    wait_started(&state, 1).await;

    let shutdown = tokio::spawn({
        let server = server.clone();
        async move { server.shutdown(LONG).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Not read: the server stopped reading requests.
    let late = t.call(ECHO, "late".into(), LONG);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!shutdown.is_finished());

    state.release.add_permits(1);
    assert_eq!(in_flight.await.unwrap(), "in flight");
    assert!(matches!(late.await, Err(RpcError::Disconnected)));
    shutdown.await.unwrap();
    assert_eq!(state.started(), 1);

    // No longer accepting.
    assert!(Transport::connect(addr).await.is_err());
}

/// Handlers still running when the grace period ends are answered with
/// `RequestTimeout`.
#[tokio::test]
async fn forced_shutdown_answers_request_timeout() {
    let (server, addr, state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let calls: Vec<_> = (0..5).map(|_| t.call(BLOCK, "x".into(), LONG)).collect();
    let quick = t.call(ECHO, "quick".into(), LONG);
    wait_started(&state, 6).await;

    server.shutdown(Duration::from_millis(50)).await;
    assert_eq!(quick.await.unwrap(), "quick");
    for reply in futures::future::join_all(calls).await {
        assert!(matches!(reply, Err(RpcError::RequestTimeout)), "{reply:?}");
    }
    let reply = t.call(ECHO, "x".into(), LONG).await;
    assert!(matches!(reply, Err(RpcError::Disconnected)), "{reply:?}");
}

/// After shutdown, handed-off connections are dropped unserved.
#[tokio::test]
async fn serve_connection_after_shutdown() {
    let state = std::sync::Arc::new(State::default());
    let server = Server::new(ServerConfig::default(), handler(state.clone()));
    server.shutdown(LONG).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(addr).await.unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    server.serve_connection(stream).await;
    assert_closed(&mut client).await;
}

/// A client that disappears mid-frame does not disturb other connections.
#[tokio::test]
async fn truncated_frame_closes_only_its_connection() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let header = Header::new(ECHO, 1, b"0123456789");
    write_frame(&mut stream, &header, b"01234").await;
    drop(stream);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(t.call(ECHO, "still".into(), LONG).await.unwrap(), "still");
    let reply = t.call(FAIL, Bytes::new(), LONG).await;
    assert!(matches!(reply, Err(RpcError::ServerError)));
}
