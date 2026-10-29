#![doc = include_str!("../docs/smp/README.md")]
//! Worker threads and cross-core execution, independent of application state.
//!
//! `start` establishes runtimes, not service readiness. Applications initialize
//! services and drain required operations before calling blocking `shutdown`.
#![forbid(unsafe_code)]

use std::{
    cell::RefCell,
    fmt,
    future::{Future, poll_fn},
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    task::Poll,
    thread::JoinHandle,
};

use crate::{Runtime, rt};

pub(crate) mod transport;
use atomic_waker::AtomicWaker;
use transport::{Inbox, Shard};

/// Logical worker index within one SMP instance, independent of hardware CPU IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CoreId(pub usize);

impl fmt::Display for CoreId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "core{}", self.0)
    }
}

/// The calling worker's logical index, if running on an SMP worker.
pub fn current_core() -> Option<CoreId> {
    rt::context::current_core()
}

#[derive(Clone, Debug)]
pub struct SmpConfig {
    /// Number of workers; zero is treated as one.
    pub cores: usize,
    /// Best-effort binding to the available CPU list, in worker order.
    pub pin: bool,
    /// Worker threads are named `<name>-<index>`.
    pub name: String,
    /// Configuration used to construct each worker's local runtime.
    pub runtime: rt::Config,
}
impl SmpConfig {
    pub fn with_cores(cores: usize) -> Self {
        Self {
            cores: cores.max(1),
            pin: true,
            name: "shep-core".into(),
            runtime: Default::default(),
        }
    }
}
impl Default for SmpConfig {
    fn default() -> Self {
        Self::with_cores(std::thread::available_parallelism().map_or(1, |n| n.get()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SmpError {
    NoSuchCore(CoreId),
    Shutdown,
    Panicked,
    Start(String),
    WorkerFailed(CoreId, String),
}
impl fmt::Display for SmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchCore(core) => write!(f, "no such core: {core}"),
            Self::Shutdown => f.write_str("SMP shut down"),
            Self::Panicked => f.write_str("submitted task panicked"),
            Self::Start(e) => write!(f, "starting SMP: {e}"),
            Self::WorkerFailed(core, e) => write!(f, "{core} failed: {e}"),
        }
    }
}
impl std::error::Error for SmpError {}

type Command = Box<dyn FnOnce() + Send>;
struct Reply<T> {
    sender: Option<mpsc::SyncSender<T>>,
    wake: Arc<AtomicWaker>,
}
impl<T> Reply<T> {
    fn complete(mut self, value: T) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(value);
        }
    }
}
impl<T> Drop for Reply<T> {
    fn drop(&mut self) {
        // Publish disconnection before waking: the receiver may immediately poll.
        drop(self.sender.take());
        self.wake.wake();
    }
}

struct ShutdownState {
    threads: Vec<JoinHandle<Result<(), SmpError>>>,
    result: Result<(), SmpError>,
}

struct Shared {
    inboxes: Vec<Arc<Inbox>>,
    // Held through joins, so every concurrent shutdown observes completion.
    shutdown: Mutex<ShutdownState>,
}
impl Drop for Shared {
    fn drop(&mut self) {
        // Last-handle drop requests stop without blocking (it may run on a worker).
        for inbox in &self.inboxes {
            inbox.close(SmpError::Shutdown);
        }
    }
}

/// Cloneable thread-safe access to a group of native runtimes.
/// Dropping the last handle requests stop; use `shutdown` to wait for teardown.
#[derive(Clone)]
pub struct Smp {
    shared: Arc<Shared>,
}
impl Smp {
    /// Start all runtimes. On failure, stop and join every thread already started.
    ///
    /// # Panics
    /// Panics on an SMP worker or inside a native runtime: startup blocks.
    pub fn start(config: SmpConfig) -> Result<Self, SmpError> {
        assert!(
            current_core().is_none() && rt::Handle::try_current().is_none(),
            "Smp::start blocks; call from outside a runtime or SMP worker"
        );
        let cpus = if config.pin {
            core_affinity::get_core_ids().unwrap_or_default()
        } else {
            vec![]
        };
        let (inboxes, ingress): (Vec<_>, Vec<_>) = (0..config.cores.max(1)).map(Inbox::new).unzip();
        let shards = Shard::mesh(&inboxes, ingress);
        let mut threads = Vec::new();
        let mut failed = None;
        for (i, shard) in shards.into_iter().enumerate() {
            let worker_inbox = inboxes[i].clone();
            let cpu = cpus.get(i).copied();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let runtime_config = config.runtime;
            let thread = std::thread::Builder::new()
                .name(format!("{}-{i}", config.name))
                .spawn(move || {
                    let _core = rt::context::enter_core(CoreId(i));
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        if let Some(cpu) = cpu {
                            let _ = core_affinity::set_for_current(cpu);
                        }
                        let mut runtime = match Runtime::new(runtime_config) {
                            Ok(runtime) => runtime,
                            Err(e) => {
                                let error = SmpError::Start(e.to_string());
                                worker_inbox.close(error.clone());
                                let _ = ready_tx.send(Err(error.clone()));
                                return Err(error);
                            }
                        };
                        let shard = Rc::new(RefCell::new(shard));
                        let transport = transport::Enter::new(shard.clone());
                        let result = runtime.run_smp(shard, || {
                            let _ = ready_tx.send(Ok(()));
                        });
                        if let Err(e) = &result {
                            worker_inbox.close(SmpError::WorkerFailed(CoreId(i), e.to_string()));
                        }
                        drop(transport);
                        let shutdown = runtime.shutdown();
                        result
                            .and(shutdown)
                            .map_err(|e| SmpError::WorkerFailed(CoreId(i), e.to_string()))
                    }))
                    .unwrap_or_else(|_| {
                        Err(SmpError::WorkerFailed(CoreId(i), "worker panicked".into()))
                    });
                    worker_inbox.close(result.clone().err().unwrap_or(SmpError::Shutdown));
                    result
                });
            match thread {
                Ok(thread) => {
                    threads.push(thread);
                }
                Err(e) => {
                    failed = Some(SmpError::Start(e.to_string()));
                    break;
                }
            }
            match ready_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    failed = Some(error);
                    break;
                }
                Err(_) => {
                    failed = Some(SmpError::Start(format!("core {i} failed to initialize")));
                    break;
                }
            }
        }
        let smp = Self {
            shared: Arc::new(Shared {
                inboxes,
                shutdown: Mutex::new(ShutdownState {
                    threads,
                    result: Ok(()),
                }),
            }),
        };
        if let Some(error) = failed {
            let _ = smp.shutdown();
            return Err(error);
        }
        Ok(smp)
    }
    pub fn cores(&self) -> usize {
        self.shared.inboxes.len()
    }
    pub fn core_ids(&self) -> impl Iterator<Item = CoreId> + use<> {
        (0..self.cores()).map(CoreId)
    }

    /// Queue on first poll. Accepted work continues if its reply is abandoned.
    /// Closures from one sender to one worker start in submission order.
    /// The future is constructed on the destination and need not be Send.
    /// Closure invocation and future polling panics return `Panicked`.
    /// Cancellation returns `Shutdown`, or the worker failure when known.
    /// Ring overflow and external ingress are unbounded; admission control belongs above SMP.
    pub fn submit_to<F, Fut, T>(
        &self,
        core: CoreId,
        f: F,
    ) -> impl Future<Output = Result<T, SmpError>> + use<F, Fut, T>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let shared = self.shared.clone();
        async move {
            let inbox = shared
                .inboxes
                .get(core.0)
                .ok_or(SmpError::NoSuchCore(core))?;
            // Exactly one result is sent, so this one-slot channel never waits for space.
            let (sender, receiver) = mpsc::sync_channel(1);
            let wake = Arc::new(AtomicWaker::new());
            let origin = Shard::origin(inbox);
            let reply = Reply {
                sender: Some(sender),
                wake: wake.clone(),
            };
            inbox.send(Box::new(move || {
                crate::spawn(async move {
                    let mut future = std::pin::pin!(async move { f().await });
                    let result = poll_fn(|cx| {
                        match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx))) {
                            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
                            Ok(Poll::Pending) => Poll::Pending,
                            Err(_) => Poll::Ready(Err(SmpError::Panicked)),
                        }
                    })
                    .await;
                    if let Some(origin) = origin {
                        // Completions share the reverse SPSC transport and its batching.
                        let _ = origin.send(Box::new(move || reply.complete(result)));
                    } else {
                        reply.complete(result);
                    }
                });
            }))?;
            poll_fn(move |cx| {
                wake.register(cx.waker());
                match receiver.try_recv() {
                    Ok(value) => Poll::Ready(value),
                    Err(mpsc::TryRecvError::Disconnected) => Poll::Ready(Err(inbox.error())),
                    Err(mpsc::TryRecvError::Empty) => Poll::Pending,
                }
            })
            .await
        }
    }

    /// Cancel work and join every worker. Applications must drain required work
    /// first. Does not flush buffers or establish durability.
    ///
    /// # Panics
    /// Panics on an SMP worker or inside a native runtime, where joining blocks progress.
    /// From other async runtimes, call on a separate ordinary thread.
    pub fn shutdown(&self) -> Result<(), SmpError> {
        assert!(
            current_core().is_none() && rt::Handle::try_current().is_none(),
            "Smp::shutdown blocks; call from outside a runtime or SMP worker"
        );
        let mut shutdown = self.shared.shutdown.lock().unwrap();
        for inbox in &self.shared.inboxes {
            inbox.close(SmpError::Shutdown);
        }
        for (i, thread) in std::mem::take(&mut shutdown.threads)
            .into_iter()
            .enumerate()
        {
            let result = thread.join().unwrap_or_else(|_| {
                Err(SmpError::WorkerFailed(
                    CoreId(i),
                    "worker panicked during teardown".into(),
                ))
            });
            if shutdown.result.is_ok() {
                shutdown.result = result;
            }
        }
        shutdown.result.clone()
    }
}
