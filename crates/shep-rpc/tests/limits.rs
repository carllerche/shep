//! Frames larger than `MAX_PAYLOAD`: senders refuse to send them, and a
//! reader that sees a header claiming one closes the connection without
//! allocating for it.

mod support;

use bytes::Bytes;
use shep_rpc::wire::{Header, MAX_PAYLOAD};
use shep_rpc::{RpcError, ServerConfig, Transport};
use support::*;
use tokio::net::TcpStream;

/// A header for `body` that claims `payload_size` bytes instead.
fn claiming(meta: u32, id: u32, payload_size: u32) -> Header {
    let mut header = Header::new(meta, id, b"");
    header.payload_size = payload_size;
    header.header_checksum = header.compute_checksum();
    header
}

/// The frame limit holds even when the memory budget would allow more.
#[tokio::test]
async fn request_claiming_more_than_the_limit_closes_connection() {
    let config = ServerConfig {
        max_memory: 1 << 30,
    };
    let (_server, addr, state) = start(config).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let header = claiming(ECHO, 1, u32::MAX);
    write_frame(&mut stream, &header, &[]).await;
    assert_closed(&mut stream).await;
    assert_eq!(state.started(), 0);
}

#[tokio::test]
async fn client_refuses_an_oversized_request() {
    let (_server, addr, _state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let body = Bytes::from(vec![0; MAX_PAYLOAD + 1]);
    let reply = t.call(ECHO, body, LONG).await;
    assert!(
        matches!(reply, Err(RpcError::PayloadTooLarge(n)) if n == MAX_PAYLOAD + 1),
        "{reply:?}"
    );
    // Refused before sending, so the connection is fine.
    assert_eq!(t.call(ECHO, "ok".into(), LONG).await.unwrap(), "ok");
}

#[tokio::test]
async fn a_request_of_exactly_the_limit_is_served() {
    let (_server, addr, _state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let body = Bytes::from(vec![7; MAX_PAYLOAD]);
    assert_eq!(t.call(ECHO, body.clone(), LONG).await.unwrap(), body);
}

/// The server answers `ServerError` rather than sending a frame the client
/// would close the connection over.
#[tokio::test]
async fn an_oversized_response_is_a_server_error() {
    let (_server, addr, _state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let reply = t.call(OVERSIZED, Bytes::new(), LONG).await;
    assert!(matches!(reply, Err(RpcError::ServerError)), "{reply:?}");
    assert_eq!(t.call(ECHO, "ok".into(), LONG).await.unwrap(), "ok");
}

/// A server whose response header claims more than the limit fails the call
/// without the client allocating for it.
#[tokio::test]
async fn response_claiming_more_than_the_limit_disconnects() {
    let addr = fake_server(|mut stream| async move {
        let (request, _) = read_frame(&mut stream).await.unwrap();
        let header = claiming(200, request.correlation_id, u32::MAX);
        write_frame(&mut stream, &header, &[]).await;
        let _ = read_frame(&mut stream).await;
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    let reply = t.call(ECHO, "ok".into(), LONG).await;
    assert!(matches!(reply, Err(RpcError::Disconnected)), "{reply:?}");
}
