//! The engine's network on shep-rpc, one per core.
//!
//! - **Replication** is pipelined: a stream keeps up to a window of
//!   AppendEntries in flight on one connection, numbered within a session, and
//!   yields their results in order (Redpanda's `buffered_protocol`).
//! - **Heartbeats** from every group on a core to one peer go out together:
//!   the first one queued opens a short window, and one RPC carries all that
//!   arrive in it (Redpanda's `heartbeat_manager`).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use futures::stream::{FuturesOrdered, StreamExt};
use shep_raft_engine::base::{BoxFuture, BoxStream};
use shep_raft_engine::errors::{
    NetworkError, RPCError, ReplicationClosed, StreamingError, Unreachable,
};
use shep_raft_engine::impls::BasicNode;
use shep_raft_engine::network::{Backoff, RPCOption, RaftNetworkFactory, RaftNetworkV2};
use shep_raft_engine::raft::{
    AppendEntriesRequest, SnapshotResponse, StreamAppendResult, TransferLeaderError,
    TransferLeaderRequest, TransferLeaderResponse, VoteRequest, VoteResponse,
};
use shep_rpc_client::{ConnectionCache, ReconnectConfig, RpcError};
use tokio::sync::oneshot;

use crate::types::{
    AppendResponse, GroupId, LogId, NodeId, RaftSnapshot, TypeConfig, Vote, WireEntry,
    from_opt_log_id, opt_log_id,
};
use crate::wire::{self, AppendReply, AppendRequest};

type PendingHeartbeat = (AppendRequest, oneshot::Sender<AppendReply>);

/// A core's connections to its peers, shared by its groups.
pub(crate) struct Peers {
    cache: ConnectionCache,
    stats: Arc<NetStats>,
    faults: Arc<Faults>,
    /// Heartbeats waiting to go, per peer.
    heartbeats: RefCell<HashMap<SocketAddr, Vec<PendingHeartbeat>>>,
    /// Makes replication sessions distinct.
    sessions: Cell<u64>,
    config: NetConfig,
}

/// Faults a test can inject into a node.
#[derive(Debug, Default)]
pub struct Faults {
    partitioned: std::sync::atomic::AtomicBool,
}

impl Faults {
    /// Cuts the node off from its Raft peers, both ways, or reconnects it.
    /// Its other services stay reachable.
    pub fn set_partitioned(&self, partitioned: bool) {
        self.partitioned.store(partitioned, Ordering::SeqCst);
    }

    pub fn is_partitioned(&self) -> bool {
        self.partitioned.load(Ordering::SeqCst)
    }
}

/// Counters for the node's outbound Raft traffic, summed over its cores.
#[derive(Debug, Default)]
pub(crate) struct NetStats {
    pub heartbeat_rpcs: AtomicU64,
    pub heartbeats: AtomicU64,
    pub append_rpcs: AtomicU64,
    /// The most AppendEntries one stream has had in flight at once.
    pub max_append_in_flight: AtomicU64,
}

#[derive(Clone, Debug)]
pub(crate) struct NetConfig {
    pub append_window: usize,
    pub heartbeat_linger: Duration,
    pub rpc_timeout: Duration,
    pub reconnect: ReconnectConfig,
}

impl Peers {
    pub(crate) fn new(config: NetConfig, stats: Arc<NetStats>, faults: Arc<Faults>) -> Peers {
        Peers {
            cache: ConnectionCache::new(config.reconnect.clone()),
            stats,
            faults,
            heartbeats: RefCell::new(HashMap::new()),
            sessions: Cell::new(0),
            config,
        }
    }

    /// A session id above every earlier one from this leader in this term:
    /// followers drop older sessions when a newer one starts. The count
    /// restarts with the process, which is safe because a restarted leader
    /// must win a new term before it leads again.
    fn next_session(&self, term: u64) -> u64 {
        let n = self.sessions.get().wrapping_add(1);
        self.sessions.set(n);
        (term << 32) | (n & 0xffff_ffff)
    }

    /// Queues a heartbeat for `addr`, to go with every other heartbeat this
    /// core queues for it within the linger window.
    fn heartbeat(
        self: &Rc<Self>,
        addr: SocketAddr,
        req: AppendRequest,
    ) -> oneshot::Receiver<AppendReply> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.heartbeats.borrow_mut();
        let batch = pending.entry(addr).or_default();
        let first = batch.is_empty();
        batch.push((req, tx));
        if first {
            let this = self.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(this.config.heartbeat_linger).await;
                let batch = this
                    .heartbeats
                    .borrow_mut()
                    .remove(&addr)
                    .unwrap_or_default();
                let (reqs, txs): (Vec<_>, Vec<_>) = batch.into_iter().unzip();
                this.stats.heartbeat_rpcs.fetch_add(1, Ordering::Relaxed);
                this.stats
                    .heartbeats
                    .fetch_add(reqs.len() as u64, Ordering::Relaxed);
                if this.faults.is_partitioned() {
                    fail_all(txs, "partitioned (injected)".into());
                    return;
                }
                let body = shep_codec::to_bytes(&wire::Heartbeats { items: reqs });
                let replies = this
                    .cache
                    .call(addr, wire::HEARTBEATS, body, this.config.rpc_timeout)
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|b| {
                        shep_codec::from_bytes::<wire::HeartbeatReplies>(b)
                            .map_err(|e| e.to_string())
                    });
                match replies {
                    Ok(r) if r.items.len() == txs.len() => {
                        for (tx, reply) in txs.into_iter().zip(r.items) {
                            let _ = tx.send(reply);
                        }
                    }
                    Ok(r) => {
                        let n = txs.len();
                        fail_all(txs, format!("{} replies to {n} heartbeats", r.items.len()))
                    }
                    Err(e) => fail_all(txs, e),
                }
            });
        }
        rx
    }
}

fn fail_all(txs: Vec<oneshot::Sender<AppendReply>>, why: String) {
    for tx in txs {
        let _ = tx.send(AppendReply::Rejected(why.clone()));
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Failure(String);

fn unreachable(why: impl ToString) -> RPCError<TypeConfig> {
    RPCError::Unreachable(Unreachable::new(&Failure(why.to_string())))
}

fn rpc_err(e: RpcError) -> RPCError<TypeConfig> {
    RPCError::Unreachable(Unreachable::new(&e))
}

pub(crate) struct NetworkFactory {
    pub(crate) group: GroupId,
    pub(crate) peers: Rc<Peers>,
}

impl RaftNetworkFactory<TypeConfig> for NetworkFactory {
    type Network = PeerClient;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> PeerClient {
        PeerClient::new(self, target, node, false)
    }

    async fn new_heartbeat_client(&mut self, target: NodeId, node: &BasicNode) -> PeerClient {
        PeerClient::new(self, target, node, true)
    }
}

/// One group's client for one peer.
pub(crate) struct PeerClient {
    group: GroupId,
    addr: Result<SocketAddr, String>,
    peers: Rc<Peers>,
    heartbeat: bool,
}

impl PeerClient {
    fn new(f: &NetworkFactory, target: NodeId, node: &BasicNode, heartbeat: bool) -> PeerClient {
        let addr = node
            .addr
            .parse::<SocketAddr>()
            .map_err(|e| format!("node {target} address {:?}: {e}", node.addr));
        PeerClient {
            group: f.group,
            addr,
            peers: f.peers.clone(),
            heartbeat,
        }
    }

    fn addr(&self) -> Result<SocketAddr, RPCError<TypeConfig>> {
        if self.peers.faults.is_partitioned() {
            return Err(unreachable("partitioned (injected)"));
        }
        self.addr.clone().map_err(unreachable)
    }

    async fn call<Req: shep_codec::Encode, Resp: shep_codec::Decode>(
        &self,
        method: u32,
        req: &Req,
        timeout: Duration,
    ) -> Result<Resp, RPCError<TypeConfig>> {
        let addr = self.addr()?;
        let body = self
            .peers
            .cache
            .call(addr, method, shep_codec::to_bytes(req), timeout)
            .await
            .map_err(rpc_err)?;
        shep_codec::from_bytes(body).map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }

    fn to_wire(
        &self,
        req: &AppendEntriesRequest<TypeConfig>,
        session: u64,
        seq: u64,
    ) -> AppendRequest {
        AppendRequest {
            group: self.group,
            session,
            seq,
            vote: (&req.vote).into(),
            prev_log_id: opt_log_id(&req.prev_log_id),
            leader_commit: opt_log_id(&req.leader_commit),
            entries: req.entries.iter().map(WireEntry::from).collect(),
        }
    }
}

/// The ids bounding a request: its prev and its last.
fn range(req: &AppendEntriesRequest<TypeConfig>) -> (Option<LogId>, Option<LogId>) {
    let last = req.entries.last().map(|e| e.log_id).or(req.prev_log_id);
    (req.prev_log_id, last)
}

/// Turns a reply into a stream item, and says whether the stream must stop
/// after it: on an error, a conflict, or a partial success short of the
/// request's end (as the engine's sequential default does).
fn stream_item(
    (prev, last): (Option<LogId>, Option<LogId>),
    reply: Result<AppendReply, RPCError<TypeConfig>>,
) -> (
    Result<StreamAppendResult<TypeConfig>, RPCError<TypeConfig>>,
    bool,
) {
    let resp = match reply.and_then(|r| r.into_response().map_err(unreachable)) {
        Ok(resp) => resp,
        Err(e) => return (Err(e), true),
    };
    let partial = matches!(&resp, AppendResponse::PartialSuccess(l) if *l != last);
    let result = resp.into_stream_result(prev, last);
    let stop = partial || result.is_err();
    (Ok(result), stop)
}

type InFlight = FuturesOrdered<
    BoxFuture<
        'static,
        (
            (Option<LogId>, Option<LogId>),
            Result<AppendReply, RPCError<TypeConfig>>,
        ),
    >,
>;

struct Pipeline<S> {
    input: S,
    input_done: bool,
    inflight: InFlight,
    stopped: bool,
    transport: shep_rpc_client::Transport,
    group: GroupId,
    session: u64,
    next_seq: u64,
    window: usize,
    timeout: Duration,
    stats: Arc<NetStats>,
    faults: Arc<Faults>,
}

impl<S> Pipeline<S>
where
    S: Stream<Item = AppendEntriesRequest<TypeConfig>> + Unpin + 'static,
{
    fn send(&mut self, req: AppendEntriesRequest<TypeConfig>) {
        let span = range(&req);
        if self.faults.is_partitioned() {
            let err = Err(unreachable("partitioned (injected)"));
            self.inflight
                .push_back(Box::pin(async move { (span, err) }));
            return;
        }
        let wire = AppendRequest {
            group: self.group,
            session: self.session,
            seq: self.next_seq,
            vote: (&req.vote).into(),
            prev_log_id: opt_log_id(&req.prev_log_id),
            leader_commit: opt_log_id(&req.leader_commit),
            entries: req.entries.iter().map(WireEntry::from).collect(),
        };
        self.next_seq += 1;
        // `call` takes the request's place on the connection now, so requests
        // go out in order whatever order their futures are polled in.
        let call = self
            .transport
            .call(wire::APPEND, shep_codec::to_bytes(&wire), self.timeout);
        self.stats.append_rpcs.fetch_add(1, Ordering::Relaxed);
        self.stats
            .max_append_in_flight
            .fetch_max(self.inflight.len() as u64 + 1, Ordering::Relaxed);
        self.inflight.push_back(Box::pin(async move {
            let reply = match call.await {
                Ok(body) => shep_codec::from_bytes::<AppendReply>(body)
                    .map_err(|e| RPCError::Network(NetworkError::new(&e))),
                Err(e) => Err(rpc_err(e)),
            };
            (span, reply)
        }));
    }

    async fn next(
        mut self,
    ) -> Option<(
        Result<StreamAppendResult<TypeConfig>, RPCError<TypeConfig>>,
        Self,
    )> {
        loop {
            if self.stopped {
                return None;
            }
            if !self.input_done && self.inflight.len() < self.window {
                tokio::select! {
                    biased;
                    done = self.inflight.next(), if !self.inflight.is_empty() => {
                        let (span, reply) = done.expect("not empty");
                        let (item, stop) = stream_item(span, reply);
                        self.stopped = stop;
                        return Some((item, self));
                    }
                    req = self.input.next() => match req {
                        Some(req) => self.send(req),
                        None => self.input_done = true,
                    },
                }
            } else {
                let (span, reply) = self.inflight.next().await?;
                let (item, stop) = stream_item(span, reply);
                self.stopped = stop;
                return Some((item, self));
            }
        }
    }
}

impl RaftNetworkV2<TypeConfig> for PeerClient {
    type SnapshotData = Bytes;

    async fn append_entries(
        &mut self,
        req: AppendEntriesRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<AppendResponse, RPCError<TypeConfig>> {
        let session = self.peers.next_session(req.vote.leader_id.term);
        let wire = self.to_wire(&req, session, 0);
        let reply: AppendReply = self.call(wire::APPEND, &wire, option.hard_ttl()).await?;
        reply.into_response().map_err(unreachable)
    }

    fn stream_append<'s, S>(
        &'s mut self,
        mut input: S,
        option: RPCOption,
    ) -> BoxFuture<
        's,
        Result<
            BoxStream<'s, Result<StreamAppendResult<TypeConfig>, RPCError<TypeConfig>>>,
            RPCError<TypeConfig>,
        >,
    >
    where
        S: Stream<Item = AppendEntriesRequest<TypeConfig>> + Unpin + 'static,
    {
        let timeout = option.hard_ttl();
        if self.heartbeat {
            // Heartbeats join their core's batch for the peer, one at a time.
            let addr = self.addr();
            let peers = self.peers.clone();
            let group = self.group;
            return Box::pin(async move {
                let addr = addr?;
                let stream = futures::stream::unfold(Some(input), move |state| {
                    let peers = peers.clone();
                    async move {
                        let mut input = state?;
                        let req = input.next().await?;
                        let span = range(&req);
                        let wire = AppendRequest {
                            group,
                            session: 0,
                            seq: 0,
                            vote: (&req.vote).into(),
                            prev_log_id: opt_log_id(&req.prev_log_id),
                            leader_commit: opt_log_id(&req.leader_commit),
                            entries: req.entries.iter().map(WireEntry::from).collect(),
                        };
                        let reply = match tokio::time::timeout(timeout, peers.heartbeat(addr, wire))
                            .await
                        {
                            Ok(Ok(reply)) => Ok(reply),
                            Ok(Err(_)) => Err(unreachable("heartbeat batch dropped")),
                            Err(_) => Err(unreachable(format!(
                                "heartbeat timed out after {timeout:?}"
                            ))),
                        };
                        let (item, stop) = stream_item(span, reply);
                        Some((item, if stop { None } else { Some(input) }))
                    }
                });
                Ok(Box::pin(stream) as BoxStream<'s, _>)
            });
        }

        let addr = self.addr();
        let peers = self.peers.clone();
        let group = self.group;
        Box::pin(async move {
            let addr = addr?;
            let transport = peers
                .cache
                .get(addr)
                .get_connected()
                .await
                .map_err(rpc_err)?;
            // The first request names the leader's term, which orders sessions.
            let Some(first) = input.next().await else {
                return Ok(Box::pin(futures::stream::empty()) as BoxStream<'s, _>);
            };
            let mut pipe = Pipeline {
                input,
                input_done: false,
                inflight: FuturesOrdered::new(),
                stopped: false,
                transport,
                group,
                session: peers.next_session(first.vote.leader_id.term),
                next_seq: 0,
                window: peers.config.append_window.max(1),
                timeout,
                stats: peers.stats.clone(),
                faults: peers.faults.clone(),
            };
            pipe.send(first);
            let stream = futures::stream::unfold(pipe, |pipe| pipe.next());
            Ok(Box::pin(stream) as BoxStream<'s, _>)
        })
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
        self.vote_call(wire::VOTE, rpc, option).await
    }

    async fn pre_vote(
        &mut self,
        rpc: VoteRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
        self.vote_call(wire::PRE_VOTE, rpc, option).await
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote,
        snapshot: RaftSnapshot,
        cancel: impl Future<Output = ReplicationClosed> + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<TypeConfig>, StreamingError<TypeConfig>> {
        let req = wire::SnapshotRequest {
            group: self.group,
            vote: (&vote).into(),
            meta: (&snapshot.meta).into(),
            data: snapshot.snapshot,
        };
        let reply: wire::SnapshotReply = tokio::select! {
            closed = cancel => return Err(StreamingError::Closed(closed)),
            r = self.call(wire::SNAPSHOT, &req, option.hard_ttl()) => r.map_err(|e| match e {
                RPCError::Unreachable(u) => StreamingError::Unreachable(u),
                RPCError::Network(n) => StreamingError::Network(n),
                RPCError::Timeout(t) => StreamingError::Timeout(t),
                other => StreamingError::Unreachable(Unreachable::new(&Failure(other.to_string()))),
            })?,
        };
        match reply {
            wire::SnapshotReply::Ok(v) => Ok(SnapshotResponse::new(Vote::from(v))),
            wire::SnapshotReply::Rejected(m) => {
                Err(StreamingError::Unreachable(Unreachable::new(&Failure(m))))
            }
        }
    }

    async fn transfer_leader(
        &mut self,
        req: TransferLeaderRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<TransferLeaderResponse<TypeConfig>, RPCError<TypeConfig>> {
        let wire = wire::TransferLeaderRequest {
            group: self.group,
            from_leader: req.from_leader().into(),
            to: *req.to_node_id(),
            last_log_id: req.last_log_id().map(Into::into),
        };
        let reply: wire::TransferLeaderReply = self
            .call(wire::TRANSFER_LEADER, &wire, option.hard_ttl())
            .await?;
        match reply {
            wire::TransferLeaderReply::Ok => Ok(Ok(())),
            wire::TransferLeaderReply::VoteChanged { expected, actual } => {
                Ok(Err(TransferLeaderError::VoteChanged {
                    expected: expected.into(),
                    actual: actual.into(),
                }))
            }
            wire::TransferLeaderReply::LogNotFlushed { expected, actual } => {
                Ok(Err(TransferLeaderError::LogNotFlushed {
                    expected: from_opt_log_id(expected),
                    actual: from_opt_log_id(actual),
                }))
            }
            wire::TransferLeaderReply::Rejected(m) => Err(unreachable(m)),
        }
    }

    fn backoff(&self) -> Option<Backoff> {
        Some(Backoff::new(std::iter::repeat(Duration::from_millis(100))))
    }
}

impl PeerClient {
    async fn vote_call(
        &self,
        method: u32,
        rpc: VoteRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
        let req = wire::VoteRequest {
            group: self.group,
            vote: (&rpc.vote).into(),
            last_log_id: opt_log_id(&rpc.last_log_id),
            leadership_transfer: rpc.leadership_transfer,
        };
        match self.call(method, &req, option.hard_ttl()).await? {
            wire::VoteReply::Ok {
                vote,
                vote_granted,
                last_log_id,
            } => Ok(VoteResponse::new(
                Vote::from(vote),
                from_opt_log_id(last_log_id),
                vote_granted,
            )),
            wire::VoteReply::Rejected(m) => Err(unreachable(m)),
        }
    }
}
