use std::io;

use shep_rpc::{Metadata, SERVICE_STATUS_MIN, Status};

/// Why a call produced no response body.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// The connection closed or broke before the response arrived. The
    /// request may or may not have been processed.
    #[error("disconnected")]
    Disconnected,
    /// No response within the call's timeout. The request may still be
    /// processed; a late response is discarded.
    #[error("timed out waiting for the response")]
    Timeout,
    /// The last connection attempt failed recently; not trying again yet.
    #[error("backing off before reconnecting")]
    Backoff,
    #[error("connecting: {0}")]
    Connect(#[source] io::Error),
    /// The request's body is larger than the client's limit; it wasn't sent.
    #[error("request body of {0} bytes is too large to send")]
    RequestTooLarge(usize),
    /// The response's body was larger than the client's limit. It was skipped
    /// unread; the connection is fine.
    #[error("response body of {0} bytes is too large")]
    ResponseTooLarge(u32),
    /// The response was compressed in a way this client doesn't support. It
    /// was skipped unread; the connection is fine.
    #[error("response compressed with unsupported scheme {0}")]
    ResponseCompression(u8),
    /// 408: the server gave up on the request, typically while shutting down.
    #[error("the server abandoned the request")]
    Abandoned,
    /// 413: the request's body was larger than the server accepts.
    #[error("the request was larger than the server accepts")]
    PayloadTooLarge,
    /// 415: the server doesn't support the request's compression.
    #[error("the server doesn't support the request's compression")]
    UnsupportedCompression,
    /// 500: the handler panicked or its response was too large to send.
    #[error("server error")]
    ServerError,
    /// A status the service defined, with the response's metadata.
    #[error("service status {status}")]
    Status { status: u32, metadata: Metadata },
    /// A transport status this client doesn't know, from a newer server.
    #[error("unknown status {0}")]
    UnknownStatus(u32),
}

impl RpcError {
    /// The error a response's status stands for, or `None` for success.
    pub(crate) fn from_status(status: u32, metadata: &Metadata) -> Option<RpcError> {
        if status >= SERVICE_STATUS_MIN {
            return Some(RpcError::Status {
                status,
                metadata: metadata.clone(),
            });
        }
        Some(match Status::from_u32(status) {
            Some(Status::Success) => return None,
            Some(Status::Abandoned) => RpcError::Abandoned,
            Some(Status::PayloadTooLarge) => RpcError::PayloadTooLarge,
            Some(Status::UnsupportedCompression) => RpcError::UnsupportedCompression,
            Some(Status::ServerError) => RpcError::ServerError,
            None => RpcError::UnknownStatus(status),
        })
    }

    /// The service's status, if the server answered with one.
    pub fn service_status(&self) -> Option<u32> {
        match self {
            RpcError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}
