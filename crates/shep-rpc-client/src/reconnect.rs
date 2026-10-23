//! Connections that come back after failing, and a cache of them by peer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use tokio::time::Instant;

use crate::error::RpcError;
use crate::transport::{Call, Reply, Transport, TransportConfig};

#[derive(Clone, Debug)]
pub struct ReconnectConfig {
    /// The wait after the first failed connection attempt. Each further
    /// failure doubles it, up to `max_backoff`; a success resets it.
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// How long one connection attempt may take.
    pub connect_timeout: Duration,
    /// For each connection made.
    pub transport: TransportConfig,
}

impl Default for ReconnectConfig {
    /// Redpanda's defaults for peer connections.
    fn default() -> ReconnectConfig {
        ReconnectConfig {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(15),
            connect_timeout: Duration::from_secs(1),
            transport: TransportConfig::default(),
        }
    }
}

/// A [`Transport`] to one address, connected on first use and replaced when
/// it fails.
///
/// After a failed attempt, calls fail fast with `Backoff` until the backoff
/// has passed, so a down peer costs callers nothing but an error. Call
/// [`reset_backoff`](ReconnectTransport::reset_backoff) on evidence the peer
/// is back (say, a request from it) to try again immediately.
pub struct ReconnectTransport {
    addr: SocketAddr,
    config: ReconnectConfig,
    state: RefCell<ReconnectState>,
    /// Held while connecting, so concurrent callers share one attempt.
    connecting: tokio::sync::Mutex<()>,
}

struct ReconnectState {
    transport: Option<Transport>,
    /// Zero until an attempt fails.
    backoff: Duration,
    last_attempt: Option<Instant>,
}

impl ReconnectState {
    fn connected(&self) -> Option<Transport> {
        self.transport.as_ref().filter(|t| !t.is_closed()).cloned()
    }

    fn backing_off(&self) -> bool {
        self.last_attempt
            .is_some_and(|at| Instant::now() < at + self.backoff)
    }
}

impl ReconnectTransport {
    pub fn new(addr: SocketAddr, config: ReconnectConfig) -> ReconnectTransport {
        ReconnectTransport {
            addr,
            config,
            state: RefCell::new(ReconnectState {
                transport: None,
                backoff: Duration::ZERO,
                last_attempt: None,
            }),
            connecting: tokio::sync::Mutex::new(()),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The current connection, connecting first if there is none.
    pub async fn get_connected(&self) -> Result<Transport, RpcError> {
        {
            let state = self.state.borrow_mut();
            if let Some(t) = state.connected() {
                return Ok(t);
            }
            if state.backing_off() {
                return Err(RpcError::Backoff);
            }
        }

        let _connecting = self.connecting.lock().await;
        {
            // Someone else may have just connected, or just failed.
            let mut state = self.state.borrow_mut();
            if let Some(t) = state.connected() {
                return Ok(t);
            }
            if state.backing_off() {
                return Err(RpcError::Backoff);
            }
            state.last_attempt = Some(Instant::now());
        }

        let connect = Transport::connect_with(self.addr, self.config.transport.clone());
        let attempt = tokio::time::timeout(self.config.connect_timeout, connect)
            .await
            .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()));
        let mut state = self.state.borrow_mut();
        match attempt {
            Ok(t) => {
                tracing::debug!(addr = %self.addr, "rpc connected");
                state.backoff = Duration::ZERO;
                state.transport = Some(t.clone());
                Ok(t)
            }
            Err(e) => {
                state.backoff = (state.backoff * 2)
                    .max(self.config.initial_backoff)
                    .min(self.config.max_backoff);
                tracing::debug!(addr = %self.addr, error = %e, backoff = ?state.backoff, "rpc connect failed");
                Err(RpcError::Connect(e))
            }
        }
    }

    /// [`Transport::call`] on the current connection, connecting if needed.
    pub async fn call(
        &self,
        method: u32,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>, RpcError> {
        self.get_connected()
            .await?
            .call(method, body, timeout)
            .await
    }

    /// [`Transport::call_with`] on the current connection, connecting if
    /// needed.
    pub async fn call_with(&self, call: Call, timeout: Duration) -> Result<Reply, RpcError> {
        self.get_connected().await?.call_with(call, timeout).await
    }

    /// Lets the next call try to connect right away.
    pub fn reset_backoff(&self) {
        let mut state = self.state.borrow_mut();
        state.backoff = Duration::ZERO;
        state.last_attempt = None;
    }

    /// Closes the current connection, failing its pending calls. The next
    /// call reconnects.
    pub fn disconnect(&self) {
        if let Some(t) = self.state.borrow_mut().transport.take() {
            t.close();
        }
    }
}

/// One [`ReconnectTransport`] per peer address, created on first use.
pub struct ConnectionCache {
    config: ReconnectConfig,
    peers: RefCell<HashMap<SocketAddr, Rc<ReconnectTransport>>>,
}

impl ConnectionCache {
    pub fn new(config: ReconnectConfig) -> ConnectionCache {
        ConnectionCache {
            config,
            peers: RefCell::new(HashMap::new()),
        }
    }

    pub fn get(&self, addr: SocketAddr) -> Rc<ReconnectTransport> {
        self.peers
            .borrow_mut()
            .entry(addr)
            .or_insert_with(|| Rc::new(ReconnectTransport::new(addr, self.config.clone())))
            .clone()
    }

    pub fn contains(&self, addr: SocketAddr) -> bool {
        self.peers.borrow_mut().contains_key(&addr)
    }

    pub async fn call(
        &self,
        addr: SocketAddr,
        method: u32,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>, RpcError> {
        self.get(addr).call(method, body, timeout).await
    }

    pub async fn call_with(
        &self,
        addr: SocketAddr,
        call: Call,
        timeout: Duration,
    ) -> Result<Reply, RpcError> {
        self.get(addr).call_with(call, timeout).await
    }

    /// See [`ReconnectTransport::reset_backoff`]. Does nothing for an
    /// address not in the cache.
    pub fn reset_backoff(&self, addr: SocketAddr) {
        if let Some(peer) = self.peers.borrow_mut().get(&addr) {
            peer.reset_backoff();
        }
    }

    /// Forgets `addr`, closing its connection and failing its pending calls.
    pub fn remove(&self, addr: SocketAddr) {
        if let Some(peer) = self.peers.borrow_mut().remove(&addr) {
            peer.disconnect();
        }
    }
}
