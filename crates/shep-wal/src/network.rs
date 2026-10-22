//! openraft's network over gRPC: each RPC names its partition, and one HTTP/2
//! connection per peer carries every partition's traffic.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openraft::error::{
    Fatal, InstallSnapshotError, NetworkError, PayloadTooLarge, RPCError, RaftError,
    ReplicationClosed, StreamingError, Unreachable,
};
use openraft::network::{Backoff, RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::{BasicNode, Snapshot, Vote};
use prost::Message;
use shep_wal_api::PartitionId;
use tonic::transport::{Channel, Endpoint};

use crate::raft_types::{self, TypeConfig, proto};

type RaftClient = proto::raft_client::RaftClient<Channel>;

/// Faults a test can inject into a node.
#[derive(Debug, Default)]
pub struct Faults {
    partitioned: AtomicBool,
}

impl Faults {
    /// Cuts the node off from its peers, in both directions, or restores it.
    /// Clients can still reach it.
    pub fn set_partitioned(&self, partitioned: bool) {
        self.partitioned.store(partitioned, Ordering::SeqCst);
    }

    pub fn is_partitioned(&self) -> bool {
        self.partitioned.load(Ordering::SeqCst)
    }
}

/// Connections to peers, shared by every partition on this node.
pub(crate) struct Peers {
    channels: Mutex<HashMap<String, Channel>>,
    max_message: usize,
}

impl Peers {
    pub(crate) fn new(max_message: usize) -> Peers {
        Peers {
            channels: Mutex::default(),
            max_message,
        }
    }

    fn client(&self, addr: &str) -> Result<RaftClient, tonic::transport::Error> {
        let mut channels = self.channels.lock().unwrap();
        if let Some(c) = channels.get(addr) {
            return Ok(self.max_sizes(RaftClient::new(c.clone())));
        }
        let channel = Endpoint::from_shared(format!("http://{addr}"))?
            .connect_timeout(Duration::from_secs(1))
            .tcp_nodelay(true)
            .connect_lazy();
        channels.insert(addr.to_string(), channel.clone());
        Ok(self.max_sizes(RaftClient::new(channel)))
    }

    fn max_sizes(&self, c: RaftClient) -> RaftClient {
        c.max_decoding_message_size(self.max_message)
            .max_encoding_message_size(self.max_message)
    }
}

/// Room left in a batch for everything but its entries.
pub(crate) const BATCH_SLACK: usize = 1024;

/// The most a repeated message field adds to an element: a tag byte and a
/// length varint.
pub(crate) const FIELD_OVERHEAD: usize = 11;

pub(crate) struct NetworkFactory {
    pub(crate) partition: PartitionId,
    pub(crate) peers: Arc<Peers>,
    pub(crate) faults: Arc<Faults>,
}

impl RaftNetworkFactory<TypeConfig> for NetworkFactory {
    type Network = Network;

    async fn new_client(&mut self, _target: u64, node: &BasicNode) -> Network {
        Network {
            partition: self.partition,
            client: self.peers.client(&node.addr).map_err(|e| e.to_string()),
            faults: self.faults.clone(),
            max_message: self.peers.max_message,
        }
    }
}

pub(crate) struct Network {
    partition: PartitionId,
    client: Result<RaftClient, String>,
    faults: Arc<Faults>,
    max_message: usize,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Failure(String);

impl Network {
    /// Sends one RPC, mapping every transport failure to `Unreachable`.
    async fn call<T, F, Fut>(&mut self, ttl: Duration, f: F) -> Result<T, Unreachable>
    where
        F: FnOnce(RaftClient) -> Fut,
        Fut: Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    {
        if self.faults.is_partitioned() {
            return Err(Unreachable::new(&Failure("partitioned (injected)".into())));
        }
        let client = self
            .client
            .clone()
            .map_err(|e| Unreachable::new(&Failure(e)))?;
        match tokio::time::timeout(ttl, f(client)).await {
            Ok(Ok(resp)) => Ok(resp.into_inner()),
            Ok(Err(status)) => Err(Unreachable::new(&status)),
            Err(_) => Err(Unreachable::new(&Failure(format!(
                "timed out after {ttl:?}"
            )))),
        }
    }
}

fn codec_err<E: std::error::Error>(e: raft_types::CodecError) -> RPCError<u64, BasicNode, E> {
    RPCError::Network(NetworkError::new(&e))
}

impl RaftNetwork<TypeConfig> for Network {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let req = proto::AppendEntriesRequest {
            partition: self.partition.0,
            vote: Some(raft_types::vote_to_proto(&rpc.vote)),
            prev_log_id: rpc.prev_log_id.as_ref().map(raft_types::log_id_to_proto),
            entries: rpc.entries.iter().map(raft_types::entry_to_proto).collect(),
            leader_commit: rpc.leader_commit.as_ref().map(raft_types::log_id_to_proto),
        };
        if req.encoded_len() > self.max_message && req.entries.len() > 1 {
            // openraft batches by count; tell it how many of these fit, and
            // it sends smaller batches. One entry always fits: the
            // proposer caps entries at half a message.
            let budget = self.max_message.saturating_sub(BATCH_SLACK);
            let mut used = 0;
            let fit = req
                .entries
                .iter()
                .take_while(|e| {
                    used += e.encoded_len() + FIELD_OVERHEAD;
                    used <= budget
                })
                .count();
            let hint = PayloadTooLarge::new_entries_hint(fit.max(1) as u64);
            return Err(RPCError::PayloadTooLarge(hint));
        }
        let resp = self
            .call(option.hard_ttl(), |mut c| async move {
                c.append_entries(req).await
            })
            .await?;
        use proto::append_entries_response::Result as R;
        Ok(match resp.result {
            Some(R::Success(_)) => AppendEntriesResponse::Success,
            Some(R::PartialSuccess(p)) => AppendEntriesResponse::PartialSuccess(
                raft_types::opt_log_id_from_proto(p.matching).map_err(codec_err)?,
            ),
            Some(R::Conflict(_)) => AppendEntriesResponse::Conflict,
            Some(R::HigherVote(v)) => AppendEntriesResponse::HigherVote(
                raft_types::vote_from_proto(Some(v)).map_err(codec_err)?,
            ),
            None => {
                return Err(codec_err(raft_types::CodecError(
                    "empty AppendEntriesResponse".into(),
                )));
            }
        })
    }

    async fn install_snapshot(
        &mut self,
        _rpc: openraft::raft::InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        openraft::raft::InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>,
    > {
        unreachable!("snapshots are sent whole, with full_snapshot")
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let req = proto::VoteRequest {
            partition: self.partition.0,
            vote: Some(raft_types::vote_to_proto(&rpc.vote)),
            last_log_id: rpc.last_log_id.as_ref().map(raft_types::log_id_to_proto),
        };
        let resp = self
            .call(option.hard_ttl(), |mut c| async move { c.vote(req).await })
            .await?;
        Ok(VoteResponse {
            vote: raft_types::vote_from_proto(resp.vote).map_err(codec_err)?,
            vote_granted: resp.vote_granted,
            last_log_id: raft_types::opt_log_id_from_proto(resp.last_log_id).map_err(codec_err)?,
        })
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote<u64>,
        snapshot: Snapshot<TypeConfig>,
        cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<u64>, StreamingError<TypeConfig, Fatal<u64>>> {
        let req = proto::InstallSnapshotRequest {
            partition: self.partition.0,
            vote: Some(raft_types::vote_to_proto(&vote)),
            meta: Some(raft_types::snapshot_meta_to_proto(&snapshot.meta)),
            data: (*snapshot.snapshot).into(),
        };
        let ttl = option.hard_ttl();
        let resp = tokio::select! {
            closed = cancel => return Err(StreamingError::Closed(closed)),
            resp = self.call(ttl, |mut c| async move { c.install_snapshot(req).await }) => resp?,
        };
        let vote = raft_types::vote_from_proto(resp.vote)
            .map_err(|e| StreamingError::Network(NetworkError::new(&e)))?;
        Ok(SnapshotResponse::new(vote))
    }

    fn backoff(&self) -> Backoff {
        Backoff::new(std::iter::repeat(Duration::from_millis(100)))
    }
}
