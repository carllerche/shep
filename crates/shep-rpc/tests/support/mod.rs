//! A test service, after Redpanda's echo service (src/v/rpc/test), and raw
//! frame helpers for poking a server byte by byte.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use shep_rpc::wire::{HEADER_SIZE, Header, MAX_PAYLOAD};
use shep_rpc::{Request, Response, Server, ServerConfig, Status, method_id};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const SERVICE: u16 = 7;
/// Returns the body.
pub const ECHO: u32 = method_id(SERVICE, 1);
/// Sleeps for the milliseconds in the body (u64 LE), then returns the body.
pub const SLEEP: u32 = method_id(SERVICE, 2);
/// Answers `ServerError`.
pub const FAIL: u32 = method_id(SERVICE, 3);
/// Panics.
pub const PANIC: u32 = method_id(SERVICE, 4);
/// Records the body in `State::log`, in the order handlers start, then
/// returns it.
pub const RECORD: u32 = method_id(SERVICE, 5);
/// Waits for a permit from `State::release`, then returns the body.
pub const BLOCK: u32 = method_id(SERVICE, 6);
/// Returns a body one byte over `MAX_PAYLOAD`.
pub const OVERSIZED: u32 = method_id(SERVICE, 7);

pub const LONG: Duration = Duration::from_secs(10);

pub struct State {
    pub log: Mutex<Vec<Bytes>>,
    /// Handlers that have started.
    pub started: AtomicUsize,
    pub release: tokio::sync::Semaphore,
}

impl Default for State {
    fn default() -> State {
        State {
            log: Mutex::default(),
            started: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

impl State {
    pub fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    pub fn log(&self) -> Vec<Bytes> {
        self.log.lock().unwrap().clone()
    }
}

pub fn handler(
    state: Arc<State>,
) -> impl Fn(Request) -> BoxFuture<'static, Response> + Clone + Send + Sync + 'static {
    move |req: Request| {
        let state = state.clone();
        Box::pin(async move {
            state.started.fetch_add(1, Ordering::SeqCst);
            match req.method {
                ECHO => Response::ok(req.body),
                SLEEP => {
                    let ms = u64::from_le_bytes(req.body[..8].try_into().unwrap());
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Response::ok(req.body)
                }
                FAIL => Response {
                    status: Status::ServerError,
                    body: Bytes::from_static(b"dropped: errors have empty bodies"),
                },
                PANIC => panic!("handler panic (expected by the test)"),
                RECORD => {
                    state.log.lock().unwrap().push(req.body.clone());
                    Response::ok(req.body)
                }
                BLOCK => {
                    state.release.acquire().await.unwrap().forget();
                    Response::ok(req.body)
                }
                OVERSIZED => Response::ok(Bytes::from(vec![0; MAX_PAYLOAD + 1])),
                _ => Response::error(Status::MethodNotFound),
            }
        }) as BoxFuture<'static, Response>
    }
}

pub fn sleep_body(ms: u64) -> Bytes {
    Bytes::copy_from_slice(&ms.to_le_bytes())
}

/// A server for the test service, accepting on a fresh loopback port.
pub async fn start(
    config: ServerConfig,
) -> (
    Server<impl Fn(Request) -> BoxFuture<'static, Response> + Clone + Send + Sync + 'static>,
    SocketAddr,
    Arc<State>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (server, state) = start_on(config, listener);
    (server, addr, state)
}

/// Like `start`, on `listener`.
pub fn start_on(
    config: ServerConfig,
    listener: TcpListener,
) -> (
    Server<impl Fn(Request) -> BoxFuture<'static, Response> + Clone + Send + Sync + 'static>,
    Arc<State>,
) {
    let state = Arc::new(State::default());
    let server = Server::new(config, handler(state.clone()));
    let serving = server.clone();
    tokio::spawn(async move { serving.serve(listener).await });
    (server, state)
}

/// Writes one frame as given, checksums and all.
pub async fn write_frame(stream: &mut TcpStream, header: &Header, body: &[u8]) {
    stream.write_all(&header.encode()).await.unwrap();
    stream.write_all(body).await.unwrap();
}

/// The next frame, or `None` if the peer closed the connection.
pub async fn read_frame(stream: &mut TcpStream) -> Option<(Header, Bytes)> {
    let mut buf = [0; HEADER_SIZE];
    stream.read_exact(&mut buf).await.ok()?;
    let header = Header::decode(&buf);
    let mut body = vec![0; header.payload_size as usize];
    stream.read_exact(&mut body).await.ok()?;
    Some((header, body.into()))
}

/// Waits for the peer to close the connection, failing if it sends anything
/// or stays open.
pub async fn assert_closed(stream: &mut TcpStream) {
    let mut buf = [0; 64];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("connection still open");
    match read {
        Ok(0) | Err(_) => {}
        Ok(n) => panic!("expected close, got {n} bytes"),
    }
}

/// A connected socket whose peer half is handed to `f`; for playing a server
/// that misbehaves.
pub async fn fake_server<F, Fut>(f: F) -> SocketAddr
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        f(stream).await;
    });
    addr
}
