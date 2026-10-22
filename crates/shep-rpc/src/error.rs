use std::io;

use crate::wire::Status;

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
    /// The body does not fit the frame's 32-bit length.
    #[error("request body of {0} bytes is too large")]
    PayloadTooLarge(usize),
    /// 404: no handler for the method id.
    #[error("method not found")]
    MethodNotFound,
    /// 408: the server gave up on the request, typically while shutting down.
    #[error("the server abandoned the request")]
    RequestTimeout,
    /// 500: the handler failed.
    #[error("server error")]
    ServerError,
    /// 503: the server cannot serve this method yet; retry.
    #[error("service unavailable")]
    ServiceUnavailable,
    /// 505: the server does not speak the request's version.
    #[error("version not supported")]
    VersionNotSupported,
    /// A status this crate does not know, from a newer peer.
    #[error("unknown status {0}")]
    UnknownStatus(u32),
}

impl RpcError {
    /// The error a response status stands for, or `None` for success.
    pub(crate) fn from_status(code: u32) -> Option<RpcError> {
        Some(match Status::from_u32(code) {
            Some(Status::Success) => return None,
            Some(Status::MethodNotFound) => RpcError::MethodNotFound,
            Some(Status::RequestTimeout) => RpcError::RequestTimeout,
            Some(Status::ServerError) => RpcError::ServerError,
            Some(Status::ServiceUnavailable) => RpcError::ServiceUnavailable,
            Some(Status::VersionNotSupported) => RpcError::VersionNotSupported,
            None => RpcError::UnknownStatus(code),
        })
    }
}
