//! Calls through a `Transport` against a real `Server` (or a fake one) over
//! loopback TCP.

#[path = "../../shep-rpc-server/tests/support/mod.rs"]
mod support;

use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use rand::seq::SliceRandom;
use shep_rpc::wire::{Header, payload_checksum};
use shep_rpc::{MetadataBuilder, SERVICE_STATUS_MIN, method_id};
use shep_rpc_client::{Call, RpcError, Transport};
use shep_rpc_server::ServerConfig;
use support::*;

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (echo_round_trip).
#[tokio::test(flavor = "local")]
async fn echo_round_trip() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let body: Vec<u8> = (0..100u8).collect::<Vec<_>>();
    let reply = t.call(ECHO, body.clone(), LONG).await.unwrap();
    assert_eq!(reply, body);
    // Empty bodies are fine too.
    assert_eq!(
        t.call(ECHO, Vec::<u8>::new(), LONG).await.unwrap(),
        Vec::<u8>::new()
    );
}

#[tokio::test(flavor = "local")]
async fn large_bodies_round_trip() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let body: Vec<u8> = (0..4 << 20).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let calls: Vec<_> = (0..4).map(|_| t.call(ECHO, body.clone(), LONG)).collect();
    for reply in futures::future::join_all(calls).await {
        assert_eq!(reply.unwrap(), body);
    }
}

#[tokio::test(flavor = "local")]
async fn many_pipelined_calls() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let calls: Vec<_> = (0..2000u32)
        .map(|i| t.call(ECHO, i.to_le_bytes().to_vec(), LONG))
        .collect();
    let replies = futures::future::join_all(calls).await;
    for (i, reply) in replies.into_iter().enumerate() {
        assert_eq!(reply.unwrap(), (i as u32).to_le_bytes().to_vec());
    }
    assert_eq!(state.started(), 2000);
}

/// Pipelining on a multi-threaded runtime, with calls from many tasks.
#[tokio::test(flavor = "local")]
async fn concurrent_calls_from_many_tasks() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let tasks: Vec<_> = (0..16u32)
        .map(|task| {
            let t = t.clone();
            tokio::task::spawn_local(async move {
                for i in 0..200u32 {
                    let body = Vec::<u8>::from(format!("{task}/{i}"));
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
#[tokio::test(flavor = "local")]
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
#[tokio::test(flavor = "local")]
async fn requests_are_written_in_call_order() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let mut calls: Vec<_> = (0..500u32)
        .map(|i| t.call(RECORD, i.to_le_bytes().to_vec(), LONG))
        .collect();
    calls.shuffle(&mut rand::rng());
    for reply in futures::future::join_all(calls).await {
        reply.unwrap();
    }
    let expected: Vec<Vec<u8>> = (0..500u32).map(|i| i.to_le_bytes().to_vec()).collect();
    assert_eq!(state.log(), expected);
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (timeout_test).
#[tokio::test(flavor = "local")]
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
#[tokio::test(flavor = "local")]
async fn late_response_is_skipped() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let reply = t
        .call(SLEEP, sleep_body(300), Duration::from_millis(50))
        .await;
    assert!(matches!(reply, Err(RpcError::Timeout)), "{reply:?}");
    // Calls go through while the late one is still running...
    assert_eq!(t.call(ECHO, "a".into(), LONG).await.unwrap(), b"a");
    // ...and after its response has come and been skipped.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(t.call(ECHO, "b".into(), LONG).await.unwrap(), b"b");
    assert!(!t.is_closed());
    assert_eq!(state.started(), 3);
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (test_cleanup_on_timeout_before_sending): calls that time out before they
/// could even be sent leave the connection healthy.
#[tokio::test(flavor = "local")]
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
        assert_eq!(t.call(ECHO, "x".into(), LONG).await.unwrap(), b"x");
    }
}

/// A call dropped before completion is forgotten; its response is skipped.
#[tokio::test(flavor = "local")]
async fn dropped_call_is_abandoned() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    // Never polled: still sent, since calls are queued when made.
    drop(t.call(SLEEP, sleep_body(50), LONG));
    let started = t.call(SLEEP, sleep_body(50), LONG);
    let _ = tokio::time::timeout(Duration::from_millis(10), started).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.call(ECHO, "x".into(), LONG).await.unwrap(), b"x");
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (missing_method_test): the service's "unknown method" status interleaved
/// with successes, and the connection stays healthy.
#[tokio::test(flavor = "local")]
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
            let status = reply.unwrap_err().service_status();
            assert_eq!(status, Some(UNKNOWN_METHOD));
        } else {
            assert_eq!(reply.unwrap(), b"testing...");
        }
    }
    assert!(!t.is_closed());
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (server_exception_test): a service's own failure status, and a panic.
#[tokio::test(flavor = "local")]
async fn handler_error() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    let reply = t.call(FAIL, Vec::<u8>::new(), LONG).await;
    assert_eq!(reply.unwrap_err().service_status(), Some(FAILED));
    let reply = t.call(PANIC, Vec::<u8>::new(), LONG).await;
    assert!(matches!(reply, Err(RpcError::ServerError)), "{reply:?}");
    assert_eq!(t.call(ECHO, "ok".into(), LONG).await.unwrap(), b"ok");
}

/// Error responses have empty bodies, whatever the handler returned.
#[tokio::test(flavor = "local")]
async fn error_responses_are_empty() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    write_frame(&mut stream, &header(FAIL, 9, b""), b"", b"").await;
    let (header, _, body) = read_frame(&mut stream).await.unwrap();
    assert!(header.checksum_ok(b""));
    assert_eq!(header.method_or_status, FAILED);
    assert_eq!(header.correlation_id, 9);
    assert_eq!(header.payload_size, 0);
    assert!(body.is_empty());
}

/// Every transport status maps to its typed error, a service's status comes
/// back with its metadata, and an unknown transport status is reported as
/// such.
#[tokio::test(flavor = "local")]
async fn statuses_map_to_errors() {
    let metadata = MetadataBuilder::new().insert(7, "leader").build().unwrap();
    let codes = [400, 408, 413, 415, 500, 599, SERVICE_STATUS_MIN + 5];
    let addr = fake_server({
        let metadata = metadata.clone();
        move |mut stream| async move {
            for code in codes {
                let (req, _, _) = read_frame(&mut stream).await.unwrap();
                let m = metadata.as_bytes();
                let header = Header::new(0, code, req.correlation_id, m, 0, payload_checksum(b""));
                write_frame(&mut stream, &header, m, b"").await;
            }
            let _ = read_frame(&mut stream).await;
        }
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    let mut errors = Vec::new();
    for _ in codes {
        errors.push(t.call(ECHO, Vec::<u8>::new(), LONG).await.unwrap_err());
    }
    match &errors[..] {
        [
            RpcError::UnknownStatus(400),
            RpcError::Abandoned,
            RpcError::PayloadTooLarge,
            RpcError::UnsupportedCompression,
            RpcError::ServerError,
            RpcError::UnknownStatus(599),
            RpcError::Status {
                status,
                metadata: m,
            },
        ] => {
            assert_eq!(*status, SERVICE_STATUS_MIN + 5);
            assert_eq!(m, &metadata);
        }
        other => panic!("{other:?}"),
    }
}

/// The version and metadata are the service's: they reach the handler as
/// sent, and the response's metadata reaches the caller.
#[tokio::test(flavor = "local")]
async fn version_and_metadata_pass_through() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let t = Transport::connect(addr).await.unwrap();
    for version in [0, 1, 7, 255] {
        let metadata = MetadataBuilder::new()
            .insert_u64(2, 5_000_000)
            .insert(1, &b"route"[..])
            .insert(0xffff, Vec::<u8>::new())
            .build()
            .unwrap();
        let call = Call::new(METADATA, Vec::<u8>::new())
            .with_version(version)
            .with_metadata(metadata.clone());
        let reply = t.call_with(call, LONG).await.unwrap();
        assert_eq!(reply.body, vec![version]);
        assert_eq!(reply.metadata, metadata);
        assert_eq!(reply.metadata.get(1), Some(&b"route"[..]));
        assert_eq!(reply.metadata.get(2), Some(&5_000_000u64.to_le_bytes()[..]));
        assert_eq!(reply.metadata.get(0xffff), Some(&b""[..]));
        assert_eq!(reply.metadata.get(3), None);
    }
}

/// A response whose metadata is malformed breaks the protocol: the client
/// closes the connection, failing every call on it.
#[tokio::test(flavor = "local")]
async fn malformed_response_metadata_closes_the_connection() {
    let addr = fake_server(|mut stream| async move {
        let (first, _, _) = read_frame(&mut stream).await.unwrap();
        // A key whose value runs past the section.
        let bad = [1, 0, 9, 0, 0];
        let h = Header::new(
            0,
            200,
            first.correlation_id,
            &bad,
            2,
            payload_checksum(b"hi"),
        );
        write_frame(&mut stream, &h, &bad, b"hi").await;
        let _ = read_frame(&mut stream).await;
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    let reply = t.call(ECHO, "a".into(), LONG).await;
    assert!(matches!(reply, Err(RpcError::Disconnected)), "{reply:?}");
    assert!(t.is_closed());
}

/// The server going away fails every pending call, and later calls fail
/// fast.
#[tokio::test(flavor = "local")]
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
#[tokio::test(flavor = "local")]
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
#[tokio::test(flavor = "local")]
async fn corrupted_response_header_closes_connection() {
    let addr = fake_server(|mut stream| async move {
        let (first, _, _) = read_frame(&mut stream).await.unwrap();
        read_frame(&mut stream).await.unwrap();
        let mut header = header(200, first.correlation_id, b"hi");
        header.header_checksum ^= 1;
        write_frame(&mut stream, &header, b"", b"hi").await;
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
#[tokio::test(flavor = "local")]
async fn corrupted_response_body_closes_connection() {
    let addr = fake_server(|mut stream| async move {
        let (req, _, _) = read_frame(&mut stream).await.unwrap();
        let header = header(200, req.correlation_id, b"hi");
        write_frame(&mut stream, &header, b"", b"ho").await;
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
#[tokio::test(flavor = "local")]
async fn response_for_unknown_id_is_skipped() {
    let addr = fake_server(|mut stream| async move {
        let (req, _, _) = read_frame(&mut stream).await.unwrap();
        let stray = header(200, req.correlation_id + 1000, b"stray body");
        write_frame(&mut stream, &stray, b"", b"stray body").await;
        let reply = header(200, req.correlation_id, b"real");
        write_frame(&mut stream, &reply, b"", b"real").await;
        let _ = read_frame(&mut stream).await;
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    assert_eq!(t.call(ECHO, "a".into(), LONG).await.unwrap(), b"real");
    assert!(!t.is_closed());
}

/// Correlation ids start at 1 and count up, one per call.
#[tokio::test(flavor = "local")]
async fn correlation_ids_are_monotonic() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let addr = fake_server(|mut stream| async move {
        while let Some((req, _, _)) = read_frame(&mut stream).await {
            assert!(req.checksum_ok(b""));
            tx.send(req.correlation_id).unwrap();
            let reply = header(200, req.correlation_id, b"");
            write_frame(&mut stream, &reply, b"", b"").await;
        }
    })
    .await;
    let t = Transport::connect(addr).await.unwrap();
    // Two time out, without their ids being reused.
    for timeout in [LONG, Duration::ZERO, LONG, Duration::ZERO, LONG] {
        let _ = t.call(ECHO, Vec::<u8>::new(), timeout).await;
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
