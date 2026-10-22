//! Serving requests: one reader per connection, handlers run concurrently,
//! responses written as they finish.

use std::future::Future;
use std::io;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::FutureExt;
use tokio::io::BufReader;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::frame::{self, Frame, IO_BUFFER};
use crate::wire::{COMPRESSION_NONE, Header, MAX_PAYLOAD, Status, VERSION};

/// How long a forced shutdown waits for the 408 responses to be written.
const FORCED_FLUSH: Duration = Duration::from_secs(1);

/// A request as the handler sees it.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: u32,
    pub body: Bytes,
}

/// A handler's answer. The body is sent only with `Success`; error responses
/// are empty, as in Redpanda.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: Status,
    pub body: Bytes,
}

impl Response {
    pub fn ok(body: impl Into<Bytes>) -> Response {
        Response {
            status: Status::Success,
            body: body.into(),
        }
    }

    pub fn error(status: Status) -> Response {
        Response {
            status,
            body: Bytes::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Bytes of request bodies held at once across all connections, from
    /// reading a body until its response is written. A connection waits for
    /// room before reading the next body, which pushes back on clients
    /// through TCP. A single request larger than this closes its connection.
    pub max_memory: usize,
}

impl Default for ServerConfig {
    fn default() -> ServerConfig {
        ServerConfig {
            max_memory: 256 << 20,
        }
    }
}

/// Serves requests with `handler`, which dispatches on the method id itself
/// (answering `MethodNotFound` for ids it does not know). A handler that
/// panics is answered with `ServerError`.
///
/// Clones share the memory budget and shutdown, so one server can serve
/// connections accepted elsewhere (say, handed from an accepting core).
pub struct Server<H> {
    handler: H,
    shared: Arc<ServerShared>,
}

impl<H: Clone> Clone for Server<H> {
    fn clone(&self) -> Server<H> {
        Server {
            handler: self.handler.clone(),
            shared: self.shared.clone(),
        }
    }
}

struct ServerShared {
    max_memory: usize,
    memory: Arc<Semaphore>,
    /// Stop accepting connections and reading requests.
    stopping: CancellationToken,
    /// Stop waiting for handlers: answer whatever is in flight with 408.
    forced: CancellationToken,
    /// Connections and handlers, so shutdown can wait for them.
    tasks: TaskTracker,
}

impl<H, Fut> Server<H>
where
    H: Fn(Request) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    pub fn new(config: ServerConfig, handler: H) -> Server<H> {
        Server {
            handler,
            shared: Arc::new(ServerShared {
                max_memory: config.max_memory,
                memory: Arc::new(Semaphore::new(config.max_memory)),
                stopping: CancellationToken::new(),
                forced: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        }
    }

    /// Accepts connections until shutdown, serving each on its own task.
    /// Shutdown waits for this to return and drop the listener.
    pub async fn serve(&self, listener: TcpListener) {
        let accepting = self.shared.tasks.token();
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = self.shared.stopping.cancelled() => break,
            };
            match accepted {
                Ok((stream, _)) => {
                    let server = self.clone();
                    self.shared
                        .tasks
                        .spawn(async move { server.serve_connection(stream).await });
                }
                Err(e) => {
                    // Typically out of file descriptors; give it a moment.
                    tracing::warn!(error = %e, "rpc accept failed");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
        drop(listener);
        drop(accepting);
    }

    /// Serves one connection until the client closes it, it breaks, or the
    /// server shuts down. Returns once every response that will be written
    /// has been.
    pub async fn serve_connection(&self, stream: TcpStream) {
        let shared = &self.shared;
        // Registered before checking, so shutdown either sees this
        // connection or this sees shutdown.
        let _token = shared.tasks.token();
        if shared.stopping.is_cancelled() {
            return;
        }
        if let Err(e) = stream.set_nodelay(true) {
            tracing::debug!(error = %e, "rpc set_nodelay failed");
        }
        let peer = stream.peer_addr().ok();
        let (read, write) = stream.into_split();
        let (tx, rx) = mpsc::unbounded_channel();
        // Cancelled when the server stops or the connection's writes fail.
        let stop = shared.stopping.child_token();

        let reading = async {
            let tx = tx;
            let mut read = BufReader::with_capacity(IO_BUFFER, read);
            tokio::select! {
                result = self.read_requests(&mut read, &tx) => {
                    if let Err(e) = result {
                        tracing::debug!(?peer, error = %e, "rpc connection closed");
                    }
                }
                _ = stop.cancelled() => {}
            }
            // Dropping `tx` here, and in each handler as it finishes, ends
            // the writer once all responses are queued.
        };
        let writing = async {
            tokio::select! {
                result = frame::write_frames(rx, write, |_| true) => {
                    if let Err(e) = result {
                        tracing::debug!(?peer, error = %e, "rpc write failed");
                        stop.cancel();
                    }
                }
                _ = async {
                    shared.forced.cancelled().await;
                    tokio::time::sleep(FORCED_FLUSH).await;
                } => {}
            }
        };
        tokio::join!(reading, writing);
    }

    /// Stops accepting and reading requests and waits for in-flight handlers
    /// to finish and their responses to be written. Handlers still running
    /// after `grace` are dropped and answered with `RequestTimeout`.
    pub async fn shutdown(&self, grace: Duration) {
        let shared = &self.shared;
        shared.stopping.cancel();
        shared.tasks.close();
        if tokio::time::timeout(grace, shared.tasks.wait())
            .await
            .is_err()
        {
            shared.forced.cancel();
            shared.tasks.wait().await;
        }
    }

    async fn read_requests(
        &self,
        read: &mut BufReader<OwnedReadHalf>,
        tx: &mpsc::UnboundedSender<Frame>,
    ) -> io::Result<()> {
        let shared = &self.shared;
        loop {
            let Some(header) = frame::read_header(read).await? else {
                return Ok(());
            };
            // Past a bad header the stream cannot be trusted to be in step.
            if !header.checksum_ok() {
                return Err(invalid("header checksum mismatch"));
            }
            if header.version != VERSION {
                frame::skip(read, header.payload_size).await?;
                // The reply carries our version, telling the client what we
                // do speak.
                let reply = Header::new(
                    Status::VersionNotSupported.as_u32(),
                    header.correlation_id,
                    &[],
                );
                let _ = tx.send(Frame {
                    header: reply,
                    body: Bytes::new(),
                    _permit: None,
                });
                continue;
            }
            if header.compression != COMPRESSION_NONE {
                return Err(invalid("unsupported compression"));
            }
            if header.payload_size as usize > shared.max_memory {
                return Err(invalid("request larger than the server's memory budget"));
            }
            let permit = shared
                .memory
                .clone()
                .acquire_many_owned(header.payload_size)
                .await
                .expect("memory semaphore is never closed");
            let body = frame::read_body(read, &header).await?;

            let handler = self.handler.clone();
            let forced = shared.forced.clone();
            let tx = tx.clone();
            let request = Request {
                method: header.meta,
                body,
            };
            shared.tasks.spawn(async move {
                let handled =
                    AssertUnwindSafe(async move { handler(request).await }).catch_unwind();
                let response = tokio::select! {
                    handled = handled => handled.unwrap_or_else(|_| {
                        tracing::error!(method = header.meta, "rpc handler panicked");
                        Response::error(Status::ServerError)
                    }),
                    _ = forced.cancelled() => Response::error(Status::RequestTimeout),
                };
                let (status, body) = match response.status {
                    Status::Success if response.body.len() > MAX_PAYLOAD => {
                        tracing::error!(
                            method = header.meta,
                            bytes = response.body.len(),
                            "rpc response larger than MAX_PAYLOAD"
                        );
                        (Status::ServerError, Bytes::new())
                    }
                    Status::Success => (Status::Success, response.body),
                    status => (status, Bytes::new()),
                };
                let reply = Header::new(status.as_u32(), header.correlation_id, &body);
                let _ = tx.send(Frame {
                    header: reply,
                    body,
                    _permit: Some(permit),
                });
            });
        }
    }
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
