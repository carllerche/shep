//! One client connection carrying many concurrent calls.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use shep_rpc::Metadata;
use shep_rpc::wire::{COMPRESSION_NONE, DEFAULT_MAX_PAYLOAD, payload_checksum};
use shep_rpc::{Connection, Frame, IO_BUFFER};

use crate::error::RpcError;

type Outcome = Result<Reply, RpcError>;

/// A request: the method, the service's version and metadata, and the body.
#[derive(Clone, Debug)]
pub struct Call {
    pub version: u8,
    pub method: u32,
    pub metadata: Metadata,
    pub body: Bytes,
}

impl Call {
    /// A call to `method` at version 0 with no metadata.
    pub fn new(method: u32, body: impl Into<Bytes>) -> Call {
        Call {
            version: 0,
            method,
            metadata: Metadata::empty(),
            body: body.into(),
        }
    }

    pub fn with_version(mut self, version: u8) -> Call {
        self.version = version;
        self
    }

    pub fn with_metadata(mut self, metadata: Metadata) -> Call {
        self.metadata = metadata;
        self
    }
}

/// A successful response.
#[derive(Clone, Debug)]
pub struct Reply {
    pub metadata: Metadata,
    pub body: Bytes,
}

#[derive(Clone, Debug)]
pub struct TransportConfig {
    /// The largest request body sent, and the largest response body read. A
    /// bigger response fails its call with `ResponseTooLarge` and is skipped
    /// unread.
    pub max_payload: usize,
}

impl Default for TransportConfig {
    fn default() -> TransportConfig {
        TransportConfig {
            max_payload: DEFAULT_MAX_PAYLOAD,
        }
    }
}

/// A connection to one server. Calls are pipelined: any number may be in
/// flight, they go out in the order `call` was invoked, and responses are
/// matched to calls by correlation id in whatever order they come back.
///
/// Once the connection fails it stays failed and every call returns
/// `Disconnected`; [`ReconnectTransport`](crate::ReconnectTransport) replaces
/// failed connections. Clones share the connection, which closes when the
/// last clone is dropped.
#[derive(Clone)]
pub struct Transport {
    conn: Rc<Conn>,
}

/// Closes the connection when the last `Transport` goes. The I/O tasks hold
/// only `Shared`, so they do not keep it open.
struct Conn {
    shared: Rc<Shared>,
    addr: SocketAddr,
    max_payload: usize,
}

struct Shared {
    state: RefCell<State>,
}

struct State {
    /// The writer's queue; `None` once the connection has failed.
    tx: Option<mpsc::UnboundedSender<Frame>>,
    /// The last correlation id handed out. Never reset, so an id is not
    /// reused while a response to its previous holder might still arrive.
    last_correlation_id: u32,
    pending: HashMap<u32, oneshot::Sender<Outcome>>,
    reader: Option<AbortHandle>,
}

impl Transport {
    pub async fn connect(addr: SocketAddr) -> std::io::Result<Transport> {
        Transport::connect_with(addr, TransportConfig::default()).await
    }

    pub async fn connect_with(
        addr: SocketAddr,
        config: TransportConfig,
    ) -> std::io::Result<Transport> {
        let max_payload = config.max_payload;
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (read, write) = stream.into_split();
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Rc::new(Shared {
            state: RefCell::new(State {
                tx: Some(tx),
                last_correlation_id: 0,
                pending: HashMap::new(),
                reader: None,
            }),
        });

        let reader = tokio::task::spawn_local(read_responses(shared.clone(), read, max_payload));
        shared.state.borrow_mut().reader = Some(reader.abort_handle());

        let writer_shared = shared.clone();
        tokio::task::spawn_local(async move {
            let requests = Connection::new(BufWriter::with_capacity(IO_BUFFER, write));
            if let Err(e) = write_requests(&writer_shared, rx, requests).await {
                tracing::debug!(%addr, error = %e, "rpc client write failed");
            }
            writer_shared.close();
        });

        Ok(Transport {
            conn: Rc::new(Conn {
                shared,
                addr,
                max_payload,
            }),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.conn.addr
    }

    /// Whether the connection has failed or been closed.
    pub fn is_closed(&self) -> bool {
        self.conn.shared.state.borrow_mut().tx.is_none()
    }

    /// Fails every pending call with `Disconnected` and closes the socket.
    pub fn close(&self) {
        self.conn.shared.close();
    }

    /// Sends `body` to `method` at version 0 with no metadata, and waits up
    /// to `timeout` for the response body. See [`Transport::call_with`].
    pub fn call(
        &self,
        method: u32,
        body: Bytes,
        timeout: Duration,
    ) -> impl Future<Output = Result<Bytes, RpcError>> + 'static {
        let reply = self.call_with(Call::new(method, body), timeout);
        async move { reply.await.map(|r| r.body) }
    }

    /// Sends `call` and waits up to `timeout` for its response. A status
    /// other than success comes back as the matching error.
    ///
    /// The request is queued, and its correlation id assigned, before this
    /// returns, not when the future is first polled. So requests go out in
    /// the order calls were made, however their futures are polled. Dropping
    /// the future abandons the call, as does dropping the last clone of the
    /// transport.
    pub fn call_with(
        &self,
        call: Call,
        timeout: Duration,
    ) -> impl Future<Output = Outcome> + 'static {
        let method = call.method;
        let queued = self.enqueue(call);
        let addr = self.conn.addr;
        async move {
            let (abandon, rx) = queued?;
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(_)) => Err(RpcError::Disconnected),
                Err(_) => {
                    let id = abandon.id;
                    tracing::debug!(%addr, method, id, ?timeout, "rpc timed out");
                    Err(RpcError::Timeout)
                }
            }
        }
    }

    fn enqueue(&self, call: Call) -> Result<(Abandon, oneshot::Receiver<Outcome>), RpcError> {
        if call.body.len() > self.conn.max_payload {
            return Err(RpcError::RequestTooLarge(call.body.len()));
        }
        // Hash outside the lock; only the cheap header checksum needs the id.
        let checksum = payload_checksum(&call.body);
        let shared = &self.conn.shared;
        let mut state = shared.state.borrow_mut();
        let state = &mut *state;
        let Some(tx) = &state.tx else {
            return Err(RpcError::Disconnected);
        };
        let mut id = state.last_correlation_id.wrapping_add(1);
        while state.pending.contains_key(&id) {
            id = id.wrapping_add(1);
        }
        state.last_correlation_id = id;
        let (reply_tx, reply_rx) = oneshot::channel();
        state.pending.insert(id, reply_tx);
        let frame = Frame::with_checksum(
            call.version,
            call.method,
            id,
            call.metadata,
            call.body,
            checksum,
        );
        // The writer only stops after `close` takes `tx`, which needs this
        // lock, so the send cannot fail here.
        let _ = tx.send(frame);
        let abandon = Abandon {
            shared: shared.clone(),
            id,
        };
        Ok((abandon, reply_rx))
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.shared.close();
    }
}

/// Forgets a call whose caller stopped waiting, so its slot does not leak
/// and a late response is skipped.
struct Abandon {
    shared: Rc<Shared>,
    id: u32,
}

impl Drop for Abandon {
    fn drop(&mut self) {
        self.shared.state.borrow_mut().pending.remove(&self.id);
    }
}

impl Shared {
    fn close(&self) {
        let (pending, reader) = {
            let mut state = self.state.borrow_mut();
            // Dropping the sender lets the writer finish and close its half.
            state.tx = None;
            (std::mem::take(&mut state.pending), state.reader.take())
        };
        if let Some(reader) = reader {
            reader.abort();
        }
        for (_, waiter) in pending {
            let _ = waiter.send(Err(RpcError::Disconnected));
        }
    }
}

/// The most requests written between flushes, so one busy connection still
/// flushes regularly.
const MAX_BATCH: usize = 512;

/// Writes requests in the order they were queued until every sender is gone,
/// flushing after each batch of whatever was queued, then shuts the write
/// side down. A call that gave up before its turn is not sent at all.
async fn write_requests(
    shared: &Shared,
    mut rx: mpsc::UnboundedReceiver<Frame>,
    mut requests: Connection<BufWriter<OwnedWriteHalf>>,
) -> std::io::Result<()> {
    let waiting = |frame: &Frame| {
        let state = shared.state.borrow_mut();
        state.pending.contains_key(&frame.header.correlation_id)
    };
    while let Some(frame) = rx.recv().await {
        let mut batch = vec![frame];
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(frame) => batch.push(frame),
                Err(_) => break,
            }
        }
        for frame in batch.into_iter().filter(|f| waiting(f)) {
            requests.queue(frame);
        }
        requests.flush().await?;
    }
    requests.shutdown().await
}

async fn read_responses(shared: Rc<Shared>, read: OwnedReadHalf, max_payload: usize) {
    let mut frames = Connection::new(BufReader::with_capacity(IO_BUFFER, read));
    loop {
        match read_response(&mut frames, max_payload).await {
            Ok(Some((correlation_id, outcome))) => {
                let waiter = shared.state.borrow_mut().pending.remove(&correlation_id);
                match waiter {
                    Some(waiter) => {
                        let _ = waiter.send(outcome);
                    }
                    // Its call timed out or was dropped; the body has already
                    // been consumed, so the stream is still in step.
                    None => tracing::debug!(
                        correlation_id,
                        "rpc response for unknown correlation id skipped"
                    ),
                }
                // A refused body is skipped only after its call has failed, so
                // the caller doesn't wait on bytes it will never get.
                if let Err(e) = frames.skip_body().await {
                    tracing::debug!(error = %e, "rpc client read failed");
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(error = %e, "rpc client read failed");
                break;
            }
        }
    }
    shared.close();
}

/// The next response and the call it answers, or `None` at end of stream. A
/// response this client can't accept fails its call and leaves its body to
/// skip (skipping a body already read does nothing); only one that breaks
/// the stream fails the connection.
async fn read_response(
    frames: &mut Connection<BufReader<OwnedReadHalf>>,
    max_payload: usize,
) -> std::io::Result<Option<(u32, Outcome)>> {
    let Some(head) = frames.next_head().await? else {
        return Ok(None);
    };
    let header = head.header;
    let refused = match &head.metadata {
        Err(e) => Some(RpcError::BadResponse(e.to_string())),
        Ok(_) if header.compression != COMPRESSION_NONE => Some(RpcError::BadResponse(format!(
            "unsupported compression {}",
            header.compression
        ))),
        Ok(_) if header.payload_size as usize > max_payload => {
            Some(RpcError::ResponseTooLarge(header.payload_size))
        }
        Ok(_) => None,
    };
    if let Some(e) = refused {
        return Ok(Some((header.correlation_id, Err(e))));
    }
    let metadata = head.metadata.expect("checked above");
    let body = frames.read_body(max_payload).await?;
    let outcome = match RpcError::from_status(header.method_or_status, &metadata) {
        None => Ok(Reply { metadata, body }),
        Some(e) => Err(e),
    };
    Ok(Some((header.correlation_id, outcome)))
}
