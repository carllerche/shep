//! Reconnecting with backoff, and the per-peer connection cache.

#[path = "../../shep-rpc-server/tests/support/mod.rs"]
mod support;

use std::net::SocketAddr;
use std::time::Duration;

use shep_rpc_client::{ConnectionCache, ReconnectConfig, ReconnectTransport, RpcError};
use shep_rpc_server::ServerConfig;
use support::*;
use tokio::net::TcpListener;

fn config(initial_ms: u64, max_ms: u64) -> ReconnectConfig {
    ReconnectConfig {
        initial_backoff: Duration::from_millis(initial_ms),
        max_backoff: Duration::from_millis(max_ms),
        connect_timeout: Duration::from_secs(1),
        transport: Default::default(),
    }
}

/// A loopback address with nothing listening on it.
async fn dead_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

/// Listens on `addr` again, once the previous listener has let go of it.
async fn rebind(addr: SocketAddr) -> TcpListener {
    for _ in 0..200 {
        if let Ok(listener) = TcpListener::bind(addr).await {
            return listener;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("could not rebind {addr}");
}

#[test]
fn default_backoff_matches_redpanda() {
    let config = ReconnectConfig::default();
    assert_eq!(config.initial_backoff, Duration::from_secs(1));
    assert_eq!(config.max_backoff, Duration::from_secs(15));
}

/// The server goes down and comes back up on the same address; the
/// transport fails, backs off, and reconnects.
#[tokio::test(flavor = "local")]
async fn reconnects_after_server_restart() {
    let (server, addr, _) = start(ServerConfig::default()).await;
    let peer = ReconnectTransport::new(addr, config(50, 200));
    assert_eq!(peer.call(ECHO, "one".into(), LONG).await.unwrap(), b"one");

    let old = peer.get_connected().await.unwrap();
    server.shutdown(LONG).await;
    while !old.is_closed() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let reply = peer.call(ECHO, "down".into(), LONG).await;
    assert!(matches!(reply, Err(RpcError::Connect(_))), "{reply:?}");
    let reply = peer.call(ECHO, "backing off".into(), LONG).await;
    assert!(matches!(reply, Err(RpcError::Backoff)), "{reply:?}");

    let (_server, _) = start_on(ServerConfig::default(), rebind(addr).await);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(peer.call(ECHO, "two".into(), LONG).await.unwrap(), b"two");
}

/// While backing off, calls fail fast without trying to connect; the
/// backoff doubles per failure up to the max.
#[tokio::test(flavor = "local")]
async fn backoff_fails_fast_and_grows() {
    let addr = dead_addr().await;
    let peer = ReconnectTransport::new(addr, config(100, 200));

    assert!(matches!(
        peer.get_connected().await,
        Err(RpcError::Connect(_))
    ));
    // Backing off 100ms.
    assert!(matches!(peer.get_connected().await, Err(RpcError::Backoff)));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(matches!(
        peer.get_connected().await,
        Err(RpcError::Connect(_))
    ));
    // Backing off 200ms now.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(matches!(peer.get_connected().await, Err(RpcError::Backoff)));
    tokio::time::sleep(Duration::from_millis(130)).await;
    assert!(matches!(
        peer.get_connected().await,
        Err(RpcError::Connect(_))
    ));
    // Capped at 200ms.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(matches!(
        peer.get_connected().await,
        Err(RpcError::Connect(_))
    ));
}

/// `reset_backoff` lets the next call try right away.
#[tokio::test(flavor = "local")]
async fn reset_backoff_retries_immediately() {
    let addr = dead_addr().await;
    let peer = ReconnectTransport::new(addr, config(60_000, 60_000));

    assert!(matches!(
        peer.get_connected().await,
        Err(RpcError::Connect(_))
    ));
    assert!(matches!(peer.get_connected().await, Err(RpcError::Backoff)));

    let (_server, _) = start_on(ServerConfig::default(), rebind(addr).await);
    assert!(matches!(peer.get_connected().await, Err(RpcError::Backoff)));
    peer.reset_backoff();
    assert_eq!(peer.call(ECHO, "back".into(), LONG).await.unwrap(), b"back");
}

/// Concurrent callers share one connection.
#[tokio::test(flavor = "local")]
async fn concurrent_callers_share_a_connection() {
    let (_server, addr, _) = start(ServerConfig::default()).await;
    let peer = ReconnectTransport::new(addr, config(50, 200));
    let calls: Vec<_> = (0..50)
        .map(|i| peer.call(ECHO, format!("{i}").into(), LONG))
        .collect();
    for (i, reply) in futures::future::join_all(calls)
        .await
        .into_iter()
        .enumerate()
    {
        assert_eq!(reply.unwrap(), format!("{i}").into_bytes());
    }
    let a = peer.get_connected().await.unwrap();
    let b = peer.get_connected().await.unwrap();
    a.close();
    assert!(b.is_closed());
}

/// `disconnect` fails pending calls; the next call reconnects.
#[tokio::test(flavor = "local")]
async fn disconnect_then_reconnect() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let peer = ReconnectTransport::new(addr, config(50, 200));
    let t = peer.get_connected().await.unwrap();
    let pending = t.call(BLOCK, "x".into(), LONG);
    while state.started() < 1 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    peer.disconnect();
    assert!(matches!(pending.await, Err(RpcError::Disconnected)));
    assert_eq!(
        peer.call(ECHO, "again".into(), LONG).await.unwrap(),
        b"again"
    );
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (echo_from_cache, basic_cache_ops): connections from the cache work, one
/// per peer, and removing a peer drops it.
#[tokio::test(flavor = "local")]
async fn connection_cache() {
    let (_a, addr_a, _) = start(ServerConfig::default()).await;
    let (_b, addr_b, _) = start(ServerConfig::default()).await;
    let cache = ConnectionCache::new(config(1, 1));
    for _ in 0..10 {
        assert_eq!(
            cache.call(addr_a, ECHO, "a".into(), LONG).await.unwrap(),
            b"a"
        );
        assert_eq!(
            cache.call(addr_b, ECHO, "b".into(), LONG).await.unwrap(),
            b"b"
        );
    }
    assert!(std::rc::Rc::ptr_eq(&cache.get(addr_a), &cache.get(addr_a)));
    assert!(cache.contains(addr_a));

    cache.remove(addr_a);
    assert!(!cache.contains(addr_a));
    assert!(cache.contains(addr_b));
    // Asking again makes a fresh entry.
    assert_eq!(
        cache.call(addr_a, ECHO, "a".into(), LONG).await.unwrap(),
        b"a"
    );
}

/// Adapted from Redpanda src/v/rpc/test/rpc_gen_cycling_test.cc
/// (rpc_abort_from_cache): removing a peer fails its outstanding calls.
#[tokio::test(flavor = "local")]
async fn cache_remove_fails_pending_calls() {
    let (_server, addr, state) = start(ServerConfig::default()).await;
    let cache = ConnectionCache::new(config(1, 1));
    let t = cache.get(addr).get_connected().await.unwrap();
    let pending = t.call(BLOCK, "x".into(), LONG);
    while state.started() < 1 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cache.remove(addr);
    assert!(matches!(pending.await, Err(RpcError::Disconnected)));
    assert!(!cache.contains(addr));
}

#[tokio::test(flavor = "local")]
async fn cache_reset_backoff() {
    let addr = dead_addr().await;
    let cache = ConnectionCache::new(config(60_000, 60_000));
    let reply = cache.call(addr, ECHO, "x".into(), LONG).await;
    assert!(matches!(reply, Err(RpcError::Connect(_))), "{reply:?}");
    let reply = cache.call(addr, ECHO, "x".into(), LONG).await;
    assert!(matches!(reply, Err(RpcError::Backoff)), "{reply:?}");
    cache.reset_backoff(addr);
    let reply = cache.call(addr, ECHO, "x".into(), LONG).await;
    assert!(matches!(reply, Err(RpcError::Connect(_))), "{reply:?}");
}
