//! Frames larger than the limit: senders refuse to send them, and a reader
//! that sees a header claiming one refuses it without allocating for it.

mod support;

use shep_rpc::wire::{DEFAULT_MAX_PAYLOAD as MAX_PAYLOAD, Header};
use shep_rpc_client::{RpcError, Transport};
use shep_rpc_server::ServerConfig;
use support::*;
use tokio::net::TcpStream;

/// A header for `body` that claims `payload_size` bytes instead.
fn claiming(meta: u32, id: u32, payload_size: u32) -> Header {
    let mut header = header(meta, id, b"");
    header.payload_size = payload_size;
    header.header_checksum = header.compute_checksum(b"");
    header
}

/// A header claiming more than the limit is answered at once, before any of
/// the body arrives, and nothing is allocated for it.
#[tokio::test(flavor = "local")]
async fn request_claiming_more_than_the_limit_is_refused_unread() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let header = claiming(ECHO, 1, u32::MAX);
    write_frame(&mut stream, &header, b"", &[]).await;
    let (reply, _, _) = read_frame(&mut stream).await.unwrap();
    assert_eq!((reply.correlation_id, reply.method_or_status), (1, 413));
    assert_eq!(state.started(), 0);
}

#[tokio::test(flavor = "local")]
async fn client_refuses_an_oversized_request() {
    let (_server, addr, _state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let body = vec![0; MAX_PAYLOAD + 1];
    let reply = t.call(ECHO, body, LONG).await;
    assert!(
        matches!(reply, Err(RpcError::RequestTooLarge(n)) if n == MAX_PAYLOAD + 1),
        "{reply:?}"
    );
    // Refused before sending, so the connection is fine.
    assert_eq!(t.call(ECHO, "ok".into(), LONG).await.unwrap(), b"ok");
}

#[tokio::test(flavor = "local")]
async fn a_request_of_exactly_the_limit_is_served() {
    let (_server, addr, _state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let body = vec![7; MAX_PAYLOAD];
    assert_eq!(t.call(ECHO, body.clone(), LONG).await.unwrap(), body);
}

/// The server answers `ServerError` rather than sending a frame the client
/// would close the connection over.
#[tokio::test(flavor = "local")]
async fn an_oversized_response_is_a_server_error() {
    let (_server, addr, _state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let reply = t.call(OVERSIZED, Vec::<u8>::new(), LONG).await;
    assert!(matches!(reply, Err(RpcError::ServerError)), "{reply:?}");
    assert_eq!(t.call(ECHO, "ok".into(), LONG).await.unwrap(), b"ok");
}

/// A response whose header claims more than the limit fails its call at
/// once, without the client allocating for it or waiting for the body.
#[tokio::test(flavor = "local")]
async fn response_claiming_more_than_the_limit_fails_the_call() {
    let addr = fake_server(|mut stream| async move {
        let (request, _, _) = read_frame(&mut stream).await.unwrap();
        let header = claiming(200, request.correlation_id, u32::MAX);
        write_frame(&mut stream, &header, b"", &[]).await;
        let _ = read_frame(&mut stream).await;
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    let reply = t.call(ECHO, "ok".into(), LONG).await;
    assert!(
        matches!(reply, Err(RpcError::ResponseTooLarge(u32::MAX))),
        "{reply:?}"
    );
}
