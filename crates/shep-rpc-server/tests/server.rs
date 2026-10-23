//! The server's side: corruption, limits, metadata, routing, serving
//! handed-off connections, and shutdown.

mod support;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use shep_rpc::MetadataBuilder;
use shep_rpc::wire::COMPRESSION_ZSTD;
use shep_rpc_client::{Call, RpcError, Transport};
use shep_rpc_server::{Request, RequestHead, Response, Server, ServerConfig, Service, service_fn};
use support::*;
use tokio::net::{TcpListener, TcpStream};

async fn wait_started(state: &State, n: usize) {
    while state.started() < n {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// One good echo over a raw socket, to show the connection was healthy.
async fn raw_echo(stream: &mut TcpStream, id: u32) {
    write_frame(stream, &header(ECHO, id, b"ok"), b"", b"ok").await;
    let (header, _, body) = read_frame(stream).await.unwrap();
    assert_eq!((header.correlation_id, header.method_or_status), (id, 200));
    assert_eq!(body, b"ok");
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (corrupted_header_at_client_test, from the server's side): a bad header
/// checksum closes the connection; new connections are fine.
#[tokio::test(flavor = "local")]
async fn corrupted_header_closes_connection() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    raw_echo(&mut stream, 1).await;

    let mut header = header(ECHO, 2, b"testing...");
    header.correlation_id = 3; // not what the checksum covered
    write_frame(&mut stream, &header, b"", b"testing...").await;
    assert_closed(&mut stream).await;
    assert_eq!(state.started(), 1);

    let t = Transport::connect(addr).await.unwrap();
    for _ in 0..10 {
        assert_eq!(
            t.call(ECHO, "testing...".into(), LONG).await.unwrap(),
            b"testing..."
        );
    }
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (corrupted_data_at_server): a body that fails its checksum closes the
/// connection before the handler sees it.
#[tokio::test(flavor = "local")]
async fn corrupted_payload_closes_connection() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    raw_echo(&mut stream, 1).await;

    let header = header(ECHO, 2, b"testing...");
    write_frame(&mut stream, &header, b"", b"testing!!!").await;
    assert_closed(&mut stream).await;
    assert_eq!(state.started(), 1);

    let t = Transport::connect(addr).await.unwrap();
    for _ in 0..10 {
        assert_eq!(
            t.call(ECHO, "testing...".into(), LONG).await.unwrap(),
            b"testing..."
        );
    }
}

/// Compression is assigned but not implemented: the request is refused
/// with 415, its body skipped, and the connection carries on.
#[tokio::test(flavor = "local")]
async fn compressed_request_is_refused() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut header = header(ECHO, 1, b"zstd?");
    header.compression = COMPRESSION_ZSTD;
    header.header_checksum = header.compute_checksum(b"");
    write_frame(&mut stream, &header, b"", b"zstd?").await;
    let (reply, _, body) = read_frame(&mut stream).await.unwrap();
    assert_eq!((reply.correlation_id, reply.method_or_status), (1, 415));
    assert!(body.is_empty());
    raw_echo(&mut stream, 2).await;
    assert_eq!(state.started(), 1);
}

/// A request over the limit is refused with 413 without being held, and the
/// connection carries on.
#[tokio::test(flavor = "local")]
async fn request_over_the_limit_is_refused() {
    let config = ServerConfig { max_payload: 1024 };
    let (_server, addr, state) = start(config).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let body = vec![7; 1025];
    write_frame(&mut stream, &header(ECHO, 1, &body), b"", &body).await;
    let (reply, _, _) = read_frame(&mut stream).await.unwrap();
    assert_eq!((reply.correlation_id, reply.method_or_status), (1, 413));
    raw_echo(&mut stream, 2).await;

    let t = Transport::connect(addr).await.unwrap();
    let reply = t.call(ECHO, body, LONG).await;
    assert!(matches!(reply, Err(RpcError::PayloadTooLarge)), "{reply:?}");
    // Exactly the limit is fine.
    let body = vec![7; 1024];
    assert_eq!(t.call(ECHO, body.clone(), LONG).await.unwrap(), body);
    assert_eq!(state.started(), 2);
}

/// A connection accepted on one core (here, a thread with its own runtime)
/// is handed to another and served there by that core's server, with
/// `serve_connection`.
#[test]
fn serve_handed_off_connection() {
    fn runtime() -> tokio::runtime::LocalRuntime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(Default::default())
            .unwrap()
    }
    let state = std::sync::Arc::new(State::default());
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
        let state = state.clone();
        move || {
            let stream = receive.recv().unwrap();
            runtime().block_on(async {
                let server = Server::new(ServerConfig::default(), service_fn(handler(state)));
                server
                    .serve_connection(TcpStream::from_std(stream).unwrap())
                    .await;
            });
        }
    });

    runtime().block_on(async {
        let t = Transport::connect(addr).await.unwrap();
        let calls: Vec<_> = (0..100u32)
            .map(|i| t.call(ECHO, Vec::<u8>::from(i.to_string()), LONG))
            .collect();
        for (i, reply) in shep_futures::future::join_all(calls)
            .await
            .into_iter()
            .enumerate()
        {
            assert_eq!(reply.unwrap(), i.to_string().into_bytes());
        }
    });
    // The transport is gone, so the connection closes and the worker ends.
    acceptor.join().unwrap();
    worker.join().unwrap();
    assert_eq!(state.started(), 100);
}

/// Shutdown stops new work, lets in-flight handlers finish and their
/// responses go out, then closes connections.
#[tokio::test(flavor = "local")]
async fn graceful_shutdown() {
    let (server, addr, state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let in_flight = t.call(BLOCK, "in flight".into(), LONG);
    wait_started(&state, 1).await;

    let shutdown = tokio::task::spawn_local({
        let server = server.clone();
        async move { server.shutdown(LONG).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Not read: the server stopped reading requests.
    let late = t.call(ECHO, "late".into(), LONG);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!shutdown.is_finished());

    state.release.add_permits(1);
    assert_eq!(in_flight.await.unwrap(), b"in flight");
    assert!(matches!(late.await, Err(RpcError::Disconnected)));
    shutdown.await.unwrap();
    assert_eq!(state.started(), 1);

    // No longer accepting.
    assert!(Transport::connect(addr).await.is_err());
}

/// Handlers still running when the grace period ends are answered with
/// `RequestTimeout`.
#[tokio::test(flavor = "local")]
async fn forced_shutdown_answers_request_timeout() {
    let (server, addr, state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let calls: Vec<_> = (0..5).map(|_| t.call(BLOCK, "x".into(), LONG)).collect();
    let quick = t.call(ECHO, "quick".into(), LONG);
    wait_started(&state, 6).await;

    server.shutdown(Duration::from_millis(50)).await;
    assert_eq!(quick.await.unwrap(), b"quick");
    for reply in shep_futures::future::join_all(calls).await {
        assert!(matches!(reply, Err(RpcError::Abandoned)), "{reply:?}");
    }
    let reply = t.call(ECHO, "x".into(), LONG).await;
    assert!(matches!(reply, Err(RpcError::Disconnected)), "{reply:?}");
}

/// After shutdown, handed-off connections are dropped unserved.
#[tokio::test(flavor = "local")]
async fn serve_connection_after_shutdown() {
    let state = std::sync::Arc::new(State::default());
    let server = Server::new(ServerConfig::default(), service_fn(handler(state.clone())));
    server.shutdown(LONG).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(addr).await.unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    server.serve_connection(stream).await;
    assert_closed(&mut client).await;
}

/// A client that disappears mid-frame does not disturb other connections.
#[tokio::test(flavor = "local")]
async fn truncated_frame_closes_only_its_connection() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let header = header(ECHO, 1, b"0123456789");
    write_frame(&mut stream, &header, b"", b"01234").await;
    drop(stream);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(t.call(ECHO, "still".into(), LONG).await.unwrap(), b"still");
    let reply = t.call(FAIL, Vec::<u8>::new(), LONG).await;
    assert_eq!(reply.unwrap_err().service_status(), Some(FAILED));
}

/// Malformed metadata breaks the protocol: the server closes the
/// connection without serving the request, whatever the checksum says.
#[tokio::test(flavor = "local")]
async fn malformed_metadata_closes_the_connection() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    for bad in [
        // A value running past the section.
        &[1, 0, 9, 0, 0][..],
        // A truncated entry header.
        &[1, 0, 0][..],
        // Keys out of order, and repeated.
        &[2, 0, 0, 0, 1, 0, 0, 0][..],
        &[1, 0, 0, 0, 1, 0, 0, 0][..],
    ] {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        raw_echo(&mut stream, 1).await;
        let h = request(ECHO, 2, bad, b"body");
        write_frame(&mut stream, &h, bad, b"body").await;
        assert_closed(&mut stream).await;
    }
    assert_eq!(state.started(), 4);
}

/// What can't be recovered from closes the connection: metadata over the
/// limit, flags this server doesn't know, and metadata that doesn't match
/// the header checksum.
#[tokio::test(flavor = "local")]
async fn unrecoverable_heads_close_the_connection() {
    let (_server, addr, state) = start(ServerConfig::default()).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut h = header(ECHO, 1, b"");
    h.metadata_size = (shep_rpc::MAX_METADATA + 1) as u16;
    h.header_checksum = h.compute_checksum(&vec![0; shep_rpc::MAX_METADATA + 1]);
    write_frame(&mut stream, &h, &vec![0; shep_rpc::MAX_METADATA + 1], b"").await;
    assert_closed(&mut stream).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut h = header(ECHO, 1, b"");
    h.flags = 1;
    h.header_checksum = h.compute_checksum(b"");
    write_frame(&mut stream, &h, b"", b"").await;
    assert_closed(&mut stream).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let good = [1, 0, 1, 0, 7];
    let h = request(ECHO, 1, &good, b"");
    write_frame(&mut stream, &h, &[1, 0, 1, 0, 8], b"").await;
    assert_closed(&mut stream).await;

    assert_eq!(state.started(), 0);
}

/// A service that routes on a metadata key: requests without it are refused
/// before their bodies are read, and the route reaches the handler.
#[derive(Clone)]
struct Routed {
    handled: Arc<AtomicUsize>,
}

const ROUTE: u16 = 1;
const NO_ROUTE: u32 = 1000;

impl Service for Routed {
    type Route = u64;

    fn route(&self, head: &RequestHead<'_>) -> Result<u64, Response> {
        let route = head.metadata.get(ROUTE).and_then(|v| v.try_into().ok());
        match route {
            Some(bytes) => Ok(u64::from_le_bytes(bytes)),
            None => Err(Response::error(NO_ROUTE)
                .with_metadata(MetadataBuilder::new().insert(9, "why").build().unwrap())),
        }
    }

    fn handle(&self, route: u64, request: Request) -> impl Future<Output = Response> + 'static {
        let handled = self.handled.clone();
        async move {
            handled.fetch_add(1, Ordering::SeqCst);
            let mut body = route.to_le_bytes().to_vec();
            body.extend_from_slice(&request.body);
            Response::ok(body)
        }
    }
}

#[tokio::test(flavor = "local")]
async fn routing_happens_before_the_body() {
    let handled = Arc::new(AtomicUsize::new(0));
    let server = Server::new(
        ServerConfig::default(),
        Routed {
            handled: handled.clone(),
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::task::spawn_local({
        let server = server.clone();
        async move { server.serve(listener).await }
    });
    let t = Transport::connect(addr).await.unwrap();

    let routed = Call::new(ECHO, "x").with_metadata(
        MetadataBuilder::new()
            .insert_u64(ROUTE, 42)
            .build()
            .unwrap(),
    );
    let reply = t.call_with(routed, LONG).await.unwrap();
    assert_eq!(&reply.body[..8], &42u64.to_le_bytes());
    assert_eq!(&reply.body[8..], b"x");

    let unrouted = Call::new(ECHO, vec![0; 1 << 20]);
    match t.call_with(unrouted, LONG).await {
        Err(RpcError::Status { status, metadata }) => {
            assert_eq!(status, NO_ROUTE);
            assert_eq!(metadata.get(9), Some(&b"why"[..]));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(handled.load(Ordering::SeqCst), 1);
    assert!(!t.is_closed());
}
