//! The gRPC services a node serves: the client protocol, admin, and the peer
//! protocol.

use std::sync::Arc;

use openraft::Snapshot;
use openraft::raft::{AppendEntriesRequest, AppendEntriesResponse, VoteRequest};
use prost::Message;
use shep_wal_api::proto as api;
use shep_wal_api::{Member, PartitionId, Proposal, Rejection};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

use crate::network::{BATCH_SLACK, FIELD_OVERHEAD};
use crate::partition::Partition;
use crate::raft_types::{self, proto};
use crate::{Inner, WalError};

/// The most entries read for one `TailResponse`.
const TAIL_BATCH: u64 = 256;

/// The most bytes of entries in one `TailResponse`, unless one entry alone is
/// bigger (it still fits in a message: entries are at most half of one).
const TAIL_BATCH_BYTES: usize = 4 << 20;

pub(crate) fn serve(
    inner: Arc<Inner>,
    listener: TcpListener,
) -> JoinHandle<Result<(), tonic::transport::Error>> {
    let mut shutdown = inner.shutdown.subscribe();
    let max_message = inner.config.max_message_size;
    let svc = Service { inner };
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(
                api::wal_server::WalServer::new(svc.clone())
                    .max_decoding_message_size(max_message)
                    .max_encoding_message_size(max_message),
            )
            .add_service(api::wal_admin_server::WalAdminServer::new(svc.clone()))
            .add_service(
                proto::raft_server::RaftServer::new(svc)
                    .max_decoding_message_size(max_message)
                    .max_encoding_message_size(max_message),
            )
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                let _ = shutdown.wait_for(|s| *s).await;
            })
            .await
    })
}

#[derive(Clone)]
struct Service {
    inner: Arc<Inner>,
}

impl Service {
    fn partition(&self, id: u64) -> Result<Arc<Partition>, Status> {
        self.inner.partition(PartitionId(id)).map_err(status)
    }

    fn peer_partition(&self, id: u64) -> Result<Arc<Partition>, Status> {
        if self.inner.faults.is_partitioned() {
            return Err(Status::unavailable("partitioned (injected)"));
        }
        self.partition(id)
    }
}

fn status(e: WalError) -> Status {
    match e {
        WalError::UnknownPartition(_) => Status::not_found(e.to_string()),
        WalError::AlreadyInitialized(_) => Status::failed_precondition(e.to_string()),
        WalError::Io(_) | WalError::Raft(_) => Status::internal(e.to_string()),
    }
}

fn bad(e: impl ToString) -> Status {
    Status::invalid_argument(e.to_string())
}

#[tonic::async_trait]
impl api::wal_server::Wal for Service {
    async fn propose(
        &self,
        req: Request<api::ProposeRequest>,
    ) -> Result<Response<api::ProposeResponse>, Status> {
        let req = req.into_inner();
        let result = match self.inner.partition(PartitionId(req.partition)) {
            Err(_) => Err(Rejection::UnknownPartition),
            Ok(p) => {
                let proposal: Proposal = req
                    .proposal
                    .ok_or_else(|| bad("missing proposal"))?
                    .try_into()
                    .map_err(bad)?;
                p.proposer.propose(proposal).await
            }
        };
        Ok(Response::new((&result).into()))
    }

    type TailStream = ReceiverStream<Result<api::TailResponse, Status>>;

    async fn tail(
        &self,
        req: Request<api::TailRequest>,
    ) -> Result<Response<Self::TailStream>, Status> {
        let req = req.into_inner();
        let p = self.partition(req.partition)?;
        let mut shutdown = self.inner.shutdown.subscribe();
        let budget = TAIL_BATCH_BYTES.min(
            self.inner
                .config
                .max_message_size
                .saturating_sub(BATCH_SLACK),
        );
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            let mut next = req.from_index;
            let mut applied = p.sm.subscribe();
            loop {
                let committed = *applied.borrow_and_update();
                if let Some(committed) = committed.filter(|&c| next <= c) {
                    let hi = committed.min(next + TAIL_BATCH - 1);
                    let entries = match p.log.read(next..=hi) {
                        Ok(entries) if entries.first().map(|e| e.log_id.index) == Some(next) => {
                            entries
                        }
                        Ok(_) => {
                            let msg = format!("index {next} is no longer in this replica's log");
                            let _ = tx.send(Err(Status::out_of_range(msg))).await;
                            return;
                        }
                        Err(e) => {
                            let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                            return;
                        }
                    };
                    let mut batch = Vec::new();
                    let mut used = 0;
                    for e in &entries {
                        let e = p.log_entry(e).to_proto();
                        used += e.encoded_len() + FIELD_OVERHEAD;
                        if used > budget && !batch.is_empty() {
                            break;
                        }
                        batch.push(e);
                    }
                    next += batch.len() as u64;
                    let resp = api::TailResponse { entries: batch };
                    if tx.send(Ok(resp)).await.is_err() {
                        return;
                    }
                    continue;
                }
                tokio::select! {
                    changed = applied.changed() => if changed.is_err() { return },
                    _ = tx.closed() => return,
                    _ = shutdown.wait_for(|s| *s) => return,
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn status(
        &self,
        req: Request<api::StatusRequest>,
    ) -> Result<Response<api::StatusResponse>, Status> {
        let p = self.partition(req.into_inner().partition)?;
        Ok(Response::new((&p.status()).into()))
    }
}

#[tonic::async_trait]
impl api::wal_admin_server::WalAdmin for Service {
    async fn create_partition(
        &self,
        req: Request<api::CreatePartitionRequest>,
    ) -> Result<Response<api::CreatePartitionResponse>, Status> {
        self.inner
            .create_partition(PartitionId(req.into_inner().partition))
            .await
            .map_err(status)?;
        Ok(Response::new(api::CreatePartitionResponse {}))
    }

    async fn initialize_partition(
        &self,
        req: Request<api::InitializePartitionRequest>,
    ) -> Result<Response<api::InitializePartitionResponse>, Status> {
        let req = req.into_inner();
        let members: Vec<Member> = req.members.into_iter().map(Into::into).collect();
        self.inner
            .initialize_partition(PartitionId(req.partition), &members)
            .await
            .map_err(status)?;
        Ok(Response::new(api::InitializePartitionResponse {}))
    }
}

#[tonic::async_trait]
impl proto::raft_server::Raft for Service {
    async fn append_entries(
        &self,
        req: Request<proto::AppendEntriesRequest>,
    ) -> Result<Response<proto::AppendEntriesResponse>, Status> {
        let req = req.into_inner();
        let p = self.peer_partition(req.partition)?;
        let rpc = AppendEntriesRequest {
            vote: raft_types::vote_from_proto(req.vote).map_err(bad)?,
            prev_log_id: raft_types::opt_log_id_from_proto(req.prev_log_id).map_err(bad)?,
            entries: req
                .entries
                .into_iter()
                .map(raft_types::entry_from_proto)
                .collect::<Result<_, _>>()
                .map_err(bad)?,
            leader_commit: raft_types::opt_log_id_from_proto(req.leader_commit).map_err(bad)?,
        };
        let resp = p
            .raft
            .append_entries(rpc)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        use proto::append_entries_response::Result as R;
        let result = match resp {
            AppendEntriesResponse::Success => R::Success(proto::Blank {}),
            AppendEntriesResponse::PartialSuccess(l) => R::PartialSuccess(proto::PartialSuccess {
                matching: l.as_ref().map(raft_types::log_id_to_proto),
            }),
            AppendEntriesResponse::Conflict => R::Conflict(proto::Blank {}),
            AppendEntriesResponse::HigherVote(v) => R::HigherVote(raft_types::vote_to_proto(&v)),
        };
        Ok(Response::new(proto::AppendEntriesResponse {
            result: Some(result),
        }))
    }

    async fn vote(
        &self,
        req: Request<proto::VoteRequest>,
    ) -> Result<Response<proto::VoteResponse>, Status> {
        let req = req.into_inner();
        let p = self.peer_partition(req.partition)?;
        let rpc = VoteRequest::new(
            raft_types::vote_from_proto(req.vote).map_err(bad)?,
            raft_types::opt_log_id_from_proto(req.last_log_id).map_err(bad)?,
        );
        let resp = p
            .raft
            .vote(rpc)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        Ok(Response::new(proto::VoteResponse {
            vote: Some(raft_types::vote_to_proto(&resp.vote)),
            vote_granted: resp.vote_granted,
            last_log_id: resp.last_log_id.as_ref().map(raft_types::log_id_to_proto),
        }))
    }

    async fn install_snapshot(
        &self,
        req: Request<proto::InstallSnapshotRequest>,
    ) -> Result<Response<proto::InstallSnapshotResponse>, Status> {
        let req = req.into_inner();
        let p = self.peer_partition(req.partition)?;
        let vote = raft_types::vote_from_proto(req.vote).map_err(bad)?;
        let meta = raft_types::snapshot_meta_from_proto(req.meta).map_err(bad)?;
        let snapshot = Snapshot {
            meta,
            snapshot: Box::new(req.data.to_vec()),
        };
        let resp = p
            .raft
            .install_full_snapshot(vote, snapshot)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        Ok(Response::new(proto::InstallSnapshotResponse {
            vote: Some(raft_types::vote_to_proto(&resp.vote)),
        }))
    }
}
