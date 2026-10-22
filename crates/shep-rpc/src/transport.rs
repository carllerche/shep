//! One client connection carrying many concurrent calls.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedReadHalf;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::error::RpcError;
use crate::frame::{self, Frame, IO_BUFFER};
use crate::wire::{COMPRESSION_NONE, Header, MAX_PAYLOAD, payload_checksum};

type Reply = Result<Bytes, RpcError>;

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
    conn: Arc<Conn>,
}

/// Closes the connection when the last `Transport` goes. The I/O tasks hold
/// only `Shared`, so they do not keep it open.
struct Conn {
    shared: Arc<Shared>,
    addr: SocketAddr,
}

struct Shared {
    state: Mutex<State>,
}

struct State {
    /// The writer's queue; `None` once the connection has failed.
    tx: Option<mpsc::UnboundedSender<Frame>>,
    /// The last correlation id handed out. Never reset, so an id is not
    /// reused while a response to its previous holder might still arrive.
    last_correlation_id: u32,
    pending: HashMap<u32, oneshot::Sender<Reply>>,
    reader: Option<AbortHandle>,
}

impl Transport {
    pub async fn connect(addr: SocketAddr) -> std::io::Result<Transport> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (read, write) = stream.into_split();
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                tx: Some(tx),
                last_correlation_id: 0,
                pending: HashMap::new(),
                reader: None,
            }),
        });

        let reader = tokio::spawn(read_responses(shared.clone(), read));
        shared.state.lock().unwrap().reader = Some(reader.abort_handle());

        let writer_shared = shared.clone();
        tokio::spawn(async move {
            // A call that timed out before its turn is not sent at all.
            let keep = |frame: &Frame| {
                let state = writer_shared.state.lock().unwrap();
                state.pending.contains_key(&frame.header.correlation_id)
            };
            if let Err(e) = frame::write_frames(rx, write, keep).await {
                tracing::debug!(%addr, error = %e, "rpc client write failed");
            }
            writer_shared.close();
        });

        Ok(Transport {
            conn: Arc::new(Conn { shared, addr }),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.conn.addr
    }

    /// Whether the connection has failed or been closed.
    pub fn is_closed(&self) -> bool {
        self.conn.shared.state.lock().unwrap().tx.is_none()
    }

    /// Fails every pending call with `Disconnected` and closes the socket.
    pub fn close(&self) {
        self.conn.shared.close();
    }

    /// Sends `body` to `method` and waits up to `timeout` for the response
    /// body. A non-success status comes back as the matching error.
    ///
    /// The request is queued, and its correlation id assigned, before this
    /// returns, not when the future is first polled. So requests go out in
    /// the order `call` was invoked, however their futures are polled.
    /// Dropping the future abandons the call, as does dropping the last
    /// clone of the transport.
    pub fn call(
        &self,
        method: u32,
        body: Bytes,
        timeout: Duration,
    ) -> impl Future<Output = Reply> + Send + 'static {
        let queued = self.enqueue(method, body);
        let addr = self.conn.addr;
        async move {
            let (abandon, rx) = queued?;
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(reply)) => reply,
                Ok(Err(_)) => Err(RpcError::Disconnected),
                Err(_) => {
                    let id = abandon.id;
                    tracing::debug!(%addr, method, id, ?timeout, "rpc timed out");
                    Err(RpcError::Timeout)
                }
            }
        }
    }

    fn enqueue(
        &self,
        method: u32,
        body: Bytes,
    ) -> Result<(Abandon, oneshot::Receiver<Reply>), RpcError> {
        if body.len() > MAX_PAYLOAD {
            return Err(RpcError::PayloadTooLarge(body.len()));
        }
        // Hash outside the lock; only the cheap header checksum needs the id.
        let checksum = payload_checksum(&body);
        let shared = &self.conn.shared;
        let mut state = shared.state.lock().unwrap();
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
        let header = Header::with_payload_checksum(method, id, body.len(), checksum);
        // The writer only stops after `close` takes `tx`, which needs this
        // lock, so the send cannot fail here.
        let _ = tx.send(Frame {
            header,
            body,
            _permit: None,
        });
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
    shared: Arc<Shared>,
    id: u32,
}

impl Drop for Abandon {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().pending.remove(&self.id);
    }
}

impl Shared {
    fn close(&self) {
        let (pending, reader) = {
            let mut state = self.state.lock().unwrap();
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

async fn read_responses(shared: Arc<Shared>, read: OwnedReadHalf) {
    let mut read = BufReader::with_capacity(IO_BUFFER, read);
    loop {
        match read_response(&mut read).await {
            Ok(Some((header, reply))) => {
                let waiter = shared
                    .state
                    .lock()
                    .unwrap()
                    .pending
                    .remove(&header.correlation_id);
                match waiter {
                    Some(waiter) => {
                        let _ = waiter.send(reply);
                    }
                    // Its call timed out or was dropped; the body has already
                    // been consumed, so the stream is still in step.
                    None => tracing::debug!(
                        correlation_id = header.correlation_id,
                        "rpc response for unknown correlation id skipped"
                    ),
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

/// The next response, or `None` at end of stream.
async fn read_response(
    read: &mut BufReader<OwnedReadHalf>,
) -> std::io::Result<Option<(Header, Reply)>> {
    let Some(header) = frame::read_header(read).await? else {
        return Ok(None);
    };
    if !header.checksum_ok() {
        return Err(invalid("header checksum mismatch"));
    }
    if header.compression != COMPRESSION_NONE {
        return Err(invalid("unsupported compression"));
    }
    let body = frame::read_body(read, &header).await?;
    let reply = match RpcError::from_status(header.meta) {
        None => Ok(body),
        Some(e) => Err(e),
    };
    Ok(Some((header, reply)))
}

fn invalid(msg: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}
