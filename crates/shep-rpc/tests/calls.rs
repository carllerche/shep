//! Calls through a `Transport` against a real `Server` (or a fake one) over
//! loopback TCP.

mod support;

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use rand::seq::SliceRandom;
use shep_rpc::wire::Header;
use shep_rpc::{RpcError, ServerConfig, Status, Transport, method_id};
use support::*;

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (echo_round_trip).
#[tokio::test]
async fn echo_round_trip() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let body: Bytes = (0..100u8).collect::<Vec<_>>().into();
    let reply = t.call(ECHO, body.clone(), LONG).await.unwrap();
    assert_eq!(reply, body);
    // Empty bodies are fine too.
    assert_eq!(
        t.call(ECHO, Bytes::new(), LONG).await.unwrap(),
        Bytes::new()
    );
}

#[tokio::test]
async fn large_bodies_round_trip() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let body: Bytes = (0..4 << 20)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>()
        .into();
    let calls: Vec<_> = (0..4).map(|_| t.call(ECHO, body.clone(), LONG)).collect();
    for reply in futures::future::join_all(calls).await {
        assert_eq!(reply.unwrap(), body);
    }
}

#[tokio::test]
async fn many_pipelined_calls() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let calls: Vec<_> = (0..2000u32)
        .map(|i| t.call(ECHO, Bytes::from(i.to_le_bytes().to_vec()), LONG))
        .collect();
    let replies = futures::future::join_all(calls).await;
    for (i, reply) in replies.into_iter().enumerate() {
        assert_eq!(
            reply.unwrap(),
            Bytes::from((i as u32).to_le_bytes().to_vec())
        );
    }
    assert_eq!(state.started(), 2000);
}

/// Pipelining on a multi-threaded runtime, with calls from many tasks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_calls_from_many_tasks() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let tasks: Vec<_> = (0..16u32)
        .map(|task| {
            let t = t.clone();
            tokio::spawn(async move {
                for i in 0..200u32 {
                    let body = Bytes::from(format!("{task}/{i}"));
                    assert_eq!(t.call(ECHO, body.clone(), LONG).await.unwrap(), body);
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
}

/// Handlers finishing in reverse order still answer the right calls.
#[tokio::test]
async fn out_of_order_responses() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let mut calls: FuturesUnordered<_> = (0..10u64)
        .map(|i| {
            let call = t.call(SLEEP, sleep_body((10 - i) * 30), LONG);
            async move { (i, call.await) }
        })
        .collect();
    let mut finished = Vec::new();
    while let Some((i, reply)) = calls.next().await {
        assert_eq!(reply.unwrap(), sleep_body((10 - i) * 30));
        finished.push(i);
    }
    assert_eq!(finished, (0..10).rev().collect::<Vec<_>>());
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (ordering_test): the server sees requests in the order they were called,
/// here even though their futures are polled in a shuffled order.
#[tokio::test]
async fn requests_are_written_in_call_order() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let mut calls: Vec<_> = (0..500u32)
        .map(|i| t.call(RECORD, Bytes::from(i.to_le_bytes().to_vec()), LONG))
        .collect();
    calls.shuffle(&mut rand::rng());
    for reply in futures::future::join_all(calls).await {
        reply.unwrap();
    }
    let expected: Vec<Bytes> = (0..500u32)
        .map(|i| Bytes::from(i.to_le_bytes().to_vec()))
        .collect();
    assert_eq!(state.log(), expected);
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (timeout_test).
#[tokio::test]
async fn timeout() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let reply = t
        .call(SLEEP, sleep_body(1000), Duration::from_millis(100))
        .await;
    assert!(matches!(reply, Err(RpcError::Timeout)), "{reply:?}");
}

/// The late response to a timed-out call is read and dropped; the connection
/// carries on.
#[tokio::test]
async fn late_response_is_skipped() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let reply = t
        .call(SLEEP, sleep_body(300), Duration::from_millis(50))
        .await;
    assert!(matches!(reply, Err(RpcError::Timeout)), "{reply:?}");
    // Calls go through while the late one is still running...
    assert_eq!(t.call(ECHO, "a".into(), LONG).await.unwrap(), "a");
    // ...and after its response has come and been skipped.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(t.call(ECHO, "b".into(), LONG).await.unwrap(), "b");
    assert!(!t.is_closed());
    assert_eq!(state.started(), 3);
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (test_cleanup_on_timeout_before_sending): calls that time out before they
/// could even be sent leave the connection healthy.
#[tokio::test]
async fn zero_timeouts_leave_connection_healthy() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let calls: Vec<_> = (0..10)
        .map(|_| t.call(SLEEP, sleep_body(100), Duration::ZERO))
        .collect();
    for reply in futures::future::join_all(calls).await {
        assert!(matches!(reply, Err(RpcError::Timeout)), "{reply:?}");
    }
    for _ in 0..10 {
        assert_eq!(t.call(ECHO, "x".into(), LONG).await.unwrap(), "x");
    }
}

/// A call dropped before completion is forgotten; its response is skipped.
#[tokio::test]
async fn dropped_call_is_abandoned() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    // Never polled: still sent, since calls are queued when made.
    drop(t.call(SLEEP, sleep_body(50), LONG));
    let started = t.call(SLEEP, sleep_body(50), LONG);
    let _ = tokio::time::timeout(Duration::from_millis(10), started).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.call(ECHO, "x".into(), LONG).await.unwrap(), "x");
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (missing_method_test): 404s interleaved with successes, and the
/// connection stays healthy.
#[tokio::test]
async fn unknown_method() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let mut kinds: Vec<bool> = (0..400).map(|i| i % 2 == 0).collect();
    kinds.shuffle(&mut rand::rng());
    let calls: Vec<_> = kinds
        .iter()
        .map(|&missing| {
            let method = if missing {
                method_id(SERVICE, 999)
            } else {
                ECHO
            };
            let call = t.call(method, "testing...".into(), LONG);
            async move { (missing, call.await) }
        })
        .collect();
    for (missing, reply) in futures::future::join_all(calls).await {
        if missing {
            assert!(matches!(reply, Err(RpcError::MethodNotFound)), "{reply:?}");
        } else {
            assert_eq!(reply.unwrap(), "testing...");
        }
    }
    assert!(!t.is_closed());
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (server_exception_test).
#[tokio::test]
async fn handler_error() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let reply = t.call(FAIL, Bytes::new(), LONG).await;
    assert!(matches!(reply, Err(RpcError::ServerError)), "{reply:?}");
    let reply = t.call(PANIC, Bytes::new(), LONG).await;
    assert!(matches!(reply, Err(RpcError::ServerError)), "{reply:?}");
    assert_eq!(t.call(ECHO, "ok".into(), LONG).await.unwrap(), "ok");
}

/// Error responses have empty bodies, whatever the handler returned.
#[tokio::test]
async fn error_responses_are_empty() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    write_frame(&mut stream, &Header::new(FAIL, 9, b""), b"").await;
    let (header, body) = read_frame(&mut stream).await.unwrap();
    assert!(header.checksum_ok());
    assert_eq!(header.meta, Status::ServerError.as_u32());
    assert_eq!(header.correlation_id, 9);
    assert_eq!(header.payload_size, 0);
    assert!(body.is_empty());
}

/// Every status maps to its typed error.
#[tokio::test]
async fn statuses_map_to_errors() {
    let codes = [404, 408, 500, 503, 505, 599];
    let addr = fake_server(move |mut stream| async move {
        for code in codes {
            let (req, _) = read_frame(&mut stream).await.unwrap();
            let header = Header::new(code, req.correlation_id, b"");
            write_frame(&mut stream, &header, b"").await;
        }
        let _ = read_frame(&mut stream).await;
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    let mut errors = Vec::new();
    for _ in codes {
        errors.push(t.call(ECHO, Bytes::new(), LONG).await.unwrap_err());
    }
    assert!(matches!(
        errors[..],
        [
            RpcError::MethodNotFound,
            RpcError::RequestTimeout,
            RpcError::ServerError,
            RpcError::ServiceUnavailable,
            RpcError::VersionNotSupported,
            RpcError::UnknownStatus(599),
        ]
    ));
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (version_not_supported): the server answers a request at another version
/// with 505 at its own version, and the connection stays usable.
#[tokio::test]
async fn version_not_supported() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    for i in 0..20u32 {
        let mut header = Header::new(ECHO, i, b"testing...");
        if i % 2 == 0 {
            header.version = 2;
            header.header_checksum = header.compute_checksum();
        }
        write_frame(&mut stream, &header, b"testing...").await;
    }
    let mut replies = Vec::new();
    for _ in 0..20 {
        let (header, body) = read_frame(&mut stream).await.unwrap();
        assert!(header.checksum_ok());
        assert_eq!(header.version, shep_rpc::wire::VERSION);
        replies.push((header.correlation_id, header.meta, body));
    }
    replies.sort_by_key(|r| r.0);
    for (id, meta, body) in replies {
        if id % 2 == 0 {
            assert_eq!(meta, Status::VersionNotSupported.as_u32());
            assert!(body.is_empty());
        } else {
            assert_eq!(meta, Status::Success.as_u32());
            assert_eq!(body, "testing...");
        }
    }
    assert_eq!(state.started(), 10);
}

/// The server going away fails every pending call, and later calls fail
/// fast.
#[tokio::test]
async fn server_drop_fails_pending_calls() {
    let addr = fake_server(|mut stream| async move {
        for _ in 0..5 {
            read_frame(&mut stream).await.unwrap();
        }
        // Drop the connection without answering.
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    let calls: Vec<_> = (0..5).map(|_| t.call(ECHO, "x".into(), LONG)).collect();
    for reply in futures::future::join_all(calls).await {
        assert!(matches!(reply, Err(RpcError::Disconnected)), "{reply:?}");
    }
    assert!(t.is_closed());
    let reply = t.call(ECHO, "x".into(), LONG).await;
    assert!(matches!(reply, Err(RpcError::Disconnected)), "{reply:?}");
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (rpc_abort_from_cache): closing the transport fails outstanding calls.
#[tokio::test]
async fn close_fails_pending_calls() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let call = t.call(SLEEP, sleep_body(100_000), LONG);
    while state.started() < 1 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    t.close();
    assert!(matches!(call.await, Err(RpcError::Disconnected)));
    assert!(t.is_closed());
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (corrupted_header_at_client_test): a response with a bad header checksum
/// closes the connection and fails everything on it.
#[tokio::test]
async fn corrupted_response_header_closes_connection() {
    let addr = fake_server(|mut stream| async move {
        let (first, _) = read_frame(&mut stream).await.unwrap();
        read_frame(&mut stream).await.unwrap();
        let mut header = Header::new(200, first.correlation_id, b"hi");
        header.header_checksum ^= 1;
        write_frame(&mut stream, &header, b"hi").await;
        let _ = read_frame(&mut stream).await;
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    let a = t.call(ECHO, "a".into(), LONG);
    let b = t.call(ECHO, "b".into(), LONG);
    assert!(matches!(a.await, Err(RpcError::Disconnected)));
    assert!(matches!(b.await, Err(RpcError::Disconnected)));
    assert!(t.is_closed());
}

/// A response body that fails its checksum also closes the connection.
#[tokio::test]
async fn corrupted_response_body_closes_connection() {
    let addr = fake_server(|mut stream| async move {
        let (req, _) = read_frame(&mut stream).await.unwrap();
        let header = Header::new(200, req.correlation_id, b"hi");
        write_frame(&mut stream, &header, b"ho").await;
        let _ = read_frame(&mut stream).await;
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    assert!(matches!(
        t.call(ECHO, "a".into(), LONG).await,
        Err(RpcError::Disconnected)
    ));
    assert!(t.is_closed());
}

/// A response for an id nobody is waiting on is skipped, body and all, and
/// the stream stays in step.
#[tokio::test]
async fn response_for_unknown_id_is_skipped() {
    let addr = fake_server(|mut stream| async move {
        let (req, _) = read_frame(&mut stream).await.unwrap();
        let stray = Header::new(200, req.correlation_id + 1000, b"stray body");
        write_frame(&mut stream, &stray, b"stray body").await;
        let reply = Header::new(200, req.correlation_id, b"real");
        write_frame(&mut stream, &reply, b"real").await;
        let _ = read_frame(&mut stream).await;
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    assert_eq!(t.call(ECHO, "a".into(), LONG).await.unwrap(), "real");
    assert!(!t.is_closed());
}

/// Correlation ids start at 1 and count up, one per call.
#[tokio::test]
async fn correlation_ids_are_monotonic() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let addr = fake_server(|mut stream| async move {
        while let Some((req, _)) = read_frame(&mut stream).await {
            assert!(req.checksum_ok());
            tx.send(req.correlation_id).unwrap();
            let reply = Header::new(200, req.correlation_id, b"");
            write_frame(&mut stream, &reply, b"").await;
        }
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    // Two time out, without their ids being reused.
    for timeout in [LONG, Duration::ZERO, LONG, Duration::ZERO, LONG] {
        let _ = t.call(ECHO, Bytes::new(), timeout).await;
    }
    let mut ids = Vec::new();
    while let Ok(id) = rx.try_recv() {
        ids.push(id);
    }
    // A call that timed out before its turn to be written is never sent.
    let sent_all = ids == [1, 2, 3, 4, 5];
    assert!(
        sent_all || ids.starts_with(&[1]) && ids.ends_with(&[5]),
        "{ids:?}"
    );
    assert!(ids.contains(&3) && ids.is_sorted(), "{ids:?}");
}
