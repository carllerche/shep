//! Serving [`shep-rpc`](shep_rpc): one reader per connection, handlers run
//! concurrently, responses written as they finish.
//!
//! A [`Service`] sees each request twice. [`Service::route`] runs as soon as
//! the header and metadata are read and checked, before the body is: it can
//! refuse the request outright, so its body is skipped without being held,
//! or decide where it will be served. [`Service::handle`] then gets the
//! whole request. [`service_fn`] makes a service from a closure that serves
//! everything.
//!
//! A server is single-threaded: its connections and handlers are tasks
//! spawned with `spawn_local`, so it runs on a
//! [`LocalRuntime`](tokio::runtime::LocalRuntime) (a core's, say), and
//! neither services nor their futures need be `Send`. To serve on several
//! cores, run a server on each.

use std::future::Future;
use std::io;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use futures::FutureExt;
use shep_rpc::wire::{COMPRESSION_NONE, DEFAULT_MAX_PAYLOAD, Header};
use shep_rpc::{Connection, Frame, IO_BUFFER};
use shep_rpc::{Metadata, Status};
use tokio::io::{BufReader, BufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// A connection's halves: requests in, responses out.
type Requests = Connection<BufReader<OwnedReadHalf>>;
type Responses = Connection<BufWriter<OwnedWriteHalf>>;

/// The most responses written between flushes, so one busy connection still
/// flushes regularly.
const MAX_BATCH: usize = 512;

/// How long a forced shutdown waits for the 408 responses to be written.
const FORCED_FLUSH: Duration = Duration::from_secs(1);

/// What a service knows about a request before its body is read.
#[derive(Debug)]
pub struct RequestHead<'a> {
    /// The service's protocol version, as the client sent it.
    pub version: u8,
    pub method: u32,
    pub metadata: &'a Metadata,
    /// Bytes of body that will be read if the request is accepted.
    pub payload_size: u32,
}

/// A request as the handler sees it.
#[derive(Debug, Clone)]
pub struct Request {
    pub version: u8,
    pub method: u32,
    pub metadata: Metadata,
    pub body: Bytes,
}

/// A handler's answer. The body is sent only with [`Status::Success`]; other
/// responses carry their status and metadata and an empty body.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u32,
    pub metadata: Metadata,
    pub body: Bytes,
}

impl Response {
    pub fn ok(body: impl Into<Bytes>) -> Response {
        Response {
            status: Status::Success.as_u32(),
            metadata: Metadata::empty(),
            body: body.into(),
        }
    }

    /// A response with `status`: one of the service's own, from
    /// [`SERVICE_STATUS_MIN`](shep_rpc::SERVICE_STATUS_MIN) up, or one of the
    /// transport's.
    pub fn error(status: u32) -> Response {
        Response {
            status,
            metadata: Metadata::empty(),
            body: Bytes::new(),
        }
    }

    pub fn with_metadata(mut self, metadata: Metadata) -> Response {
        self.metadata = metadata;
        self
    }
}

/// What a server serves.
pub trait Service: Clone + 'static {
    /// What [`route`](Service::route) decided, handed to
    /// [`handle`](Service::handle).
    type Route: 'static;

    /// Decides from the head alone whether to serve the request, on the
    /// connection's task, before its body is read. An `Err` is sent as the
    /// response (its body ignored), and the request's body is skipped.
    fn route(&self, head: &RequestHead<'_>) -> Result<Self::Route, Response>;

    /// Serves the request. A handler that panics is answered with
    /// [`Status::ServerError`].
    fn handle(
        &self,
        route: Self::Route,
        request: Request,
    ) -> impl Future<Output = Response> + 'static;
}

/// A [`Service`] that routes every request to `handler`.
pub fn service_fn<H, Fut>(handler: H) -> ServiceFn<H>
where
    H: Fn(Request) -> Fut + Clone + 'static,
    Fut: Future<Output = Response> + 'static,
{
    ServiceFn(handler)
}

#[derive(Clone)]
pub struct ServiceFn<H>(H);

impl<H, Fut> Service for ServiceFn<H>
where
    H: Fn(Request) -> Fut + Clone + 'static,
    Fut: Future<Output = Response> + 'static,
{
    type Route = ();

    fn route(&self, _: &RequestHead<'_>) -> Result<(), Response> {
        Ok(())
    }

    fn handle(&self, _: (), request: Request) -> impl Future<Output = Response> + 'static {
        (self.0)(request)
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// The largest request body served, and the largest response body sent.
    /// A bigger request is answered [`Status::PayloadTooLarge`] and its body
    /// skipped; a bigger response becomes [`Status::ServerError`].
    pub max_payload: usize,
}

impl Default for ServerConfig {
    fn default() -> ServerConfig {
        ServerConfig {
            max_payload: DEFAULT_MAX_PAYLOAD,
        }
    }
}

/// Serves requests with a [`Service`].
///
/// Clones share shutdown, so a server can serve connections accepted
/// elsewhere on its thread, as well as its own listener's.
pub struct Server<S> {
    service: S,
    shared: Rc<ServerShared>,
}

impl<S: Clone> Clone for Server<S> {
    fn clone(&self) -> Server<S> {
        Server {
            service: self.service.clone(),
            shared: self.shared.clone(),
        }
    }
}

struct ServerShared {
    max_payload: usize,
    /// Stop accepting connections and reading requests.
    stopping: CancellationToken,
    /// Stop waiting for handlers: answer whatever is in flight with 408.
    forced: CancellationToken,
    /// Connections and handlers, so shutdown can wait for them.
    tasks: TaskTracker,
}

impl<S: Service> Server<S> {
    pub fn new(config: ServerConfig, service: S) -> Server<S> {
        Server {
            service,
            shared: Rc::new(ServerShared {
                max_payload: config.max_payload,
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
                        .spawn_local(async move { server.serve_connection(stream).await });
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
            let mut frames = Connection::new(BufReader::with_capacity(IO_BUFFER, read));
            tokio::select! {
                result = self.read_requests(&mut frames, &tx) => {
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
                result = write_responses(rx, Connection::new(BufWriter::with_capacity(IO_BUFFER, write))) => {
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
    /// after `grace` are dropped and answered with [`Status::Abandoned`].
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
        frames: &mut Requests,
        tx: &mpsc::UnboundedSender<Frame>,
    ) -> io::Result<()> {
        let shared = &self.shared;
        loop {
            let Some(head) = frames.next_head().await? else {
                return Ok(());
            };
            let header = head.header;
            let metadata = match head.metadata {
                Ok(metadata) => metadata,
                Err(e) => {
                    tracing::debug!(error = %e, "rpc request metadata malformed");
                    refuse(
                        frames,
                        tx,
                        &header,
                        Response::error(Status::BadRequest.as_u32()),
                    )
                    .await?;
                    continue;
                }
            };
            if header.compression != COMPRESSION_NONE {
                refuse(
                    frames,
                    tx,
                    &header,
                    Response::error(Status::UnsupportedCompression.as_u32()),
                )
                .await?;
                continue;
            }
            if header.payload_size as usize > shared.max_payload {
                refuse(
                    frames,
                    tx,
                    &header,
                    Response::error(Status::PayloadTooLarge.as_u32()),
                )
                .await?;
                continue;
            }
            let route = self.service.route(&RequestHead {
                version: header.version,
                method: header.method_or_status,
                metadata: &metadata,
                payload_size: header.payload_size,
            });
            let route = match route {
                Ok(route) => route,
                Err(response) => {
                    refuse(frames, tx, &header, response).await?;
                    continue;
                }
            };
            let body = frames.read_body(shared.max_payload).await?;

            let service = self.service.clone();
            let max_payload = shared.max_payload;
            let forced = shared.forced.clone();
            let tx = tx.clone();
            let method = header.method_or_status;
            let request = Request {
                version: header.version,
                method,
                metadata,
                body,
            };
            shared.tasks.spawn_local(async move {
                let handled = AssertUnwindSafe(service.handle(route, request)).catch_unwind();
                let response = tokio::select! {
                    handled = handled => handled.unwrap_or_else(|_| {
                        tracing::error!(method, "rpc handler panicked");
                        Response::error(Status::ServerError.as_u32())
                    }),
                    _ = forced.cancelled() => Response::error(Status::Abandoned.as_u32()),
                };
                let ok = Status::Success.as_u32();
                let (status, body) = match response.status {
                    status if status == ok && response.body.len() > max_payload => {
                        tracing::error!(
                            method,
                            bytes = response.body.len(),
                            "rpc response larger than the limit"
                        );
                        (Status::ServerError.as_u32(), Bytes::new())
                    }
                    status if status == ok => (status, response.body),
                    status => (status, Bytes::new()),
                };
                let _ = tx.send(reply(&header, status, response.metadata, body));
            });
        }
    }
}

/// Answers the request `header` heads with `response`, and skips its body
/// without holding it.
async fn refuse(
    frames: &mut Requests,
    tx: &mpsc::UnboundedSender<Frame>,
    header: &Header,
    response: Response,
) -> io::Result<()> {
    let _ = tx.send(reply(
        header,
        response.status,
        response.metadata,
        Bytes::new(),
    ));
    frames.skip_body().await
}

/// Writes responses as handlers finish them until every sender is gone,
/// flushing after each batch of whatever was queued, then shuts the write
/// side down.
async fn write_responses(
    mut rx: mpsc::UnboundedReceiver<Frame>,
    mut responses: Responses,
) -> io::Result<()> {
    while let Some(frame) = rx.recv().await {
        responses.queue(frame);
        while responses.queued() < MAX_BATCH {
            match rx.try_recv() {
                Ok(frame) => responses.queue(frame),
                Err(_) => break,
            }
        }
        responses.flush().await?;
    }
    responses.shutdown().await
}

/// The response to the request `header` heads, at the request's version.
fn reply(header: &Header, status: u32, metadata: Metadata, body: Bytes) -> Frame {
    Frame::new(
        header.version,
        status,
        header.correlation_id,
        metadata,
        body,
    )
}
