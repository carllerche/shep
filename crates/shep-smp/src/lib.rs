//! Thread-per-core, after Seastar's `smp` (design §3, §9.6).
//!
//! An [`Smp`] runs one OS thread per core, each with its own tokio
//! `current_thread` runtime. Work on a core is ordinary async code with no
//! locks: state a core owns is `Rc`/`RefCell`, and futures needn't be `Send`.
//! Cores talk only by message: [`Smp::submit_to`] runs a closure on another
//! core and returns its result, through a bounded mailbox per core, so a slow
//! core pushes back on its senders instead of queueing without bound.
//!
//! [`Sharded`] is Seastar's `sharded<T>`: one instance of a service per core,
//! reached locally with [`Sharded::local`] or on another core with
//! [`Sharded::invoke_on`].
//!
//! A single core is a complete deployment: everything runs, just on one
//! thread.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tokio::sync::{mpsc, oneshot};
use tokio::task::LocalSet;

/// A core of an [`Smp`], numbered from 0.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CoreId(pub usize);

impl fmt::Display for CoreId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "core{}", self.0)
    }
}

#[derive(Clone, Debug)]
pub struct SmpConfig {
    /// How many cores, at least 1.
    pub cores: usize,
    /// Messages a core's mailbox holds before senders wait.
    pub mailbox_capacity: usize,
    /// Pin core `i`'s thread to the machine's `i`th CPU, where the platform
    /// allows it. Best effort: unsupported platforms run unpinned.
    pub pin: bool,
    /// Threads are named `<name>-<core>`.
    pub name: String,
}

impl SmpConfig {
    /// One core per available CPU.
    pub fn new() -> SmpConfig {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        SmpConfig::with_cores(cores)
    }

    pub fn with_cores(cores: usize) -> SmpConfig {
        SmpConfig {
            cores: cores.max(1),
            mailbox_capacity: 1024,
            pin: true,
            name: "shep-core".into(),
        }
    }
}

impl Default for SmpConfig {
    fn default() -> SmpConfig {
        SmpConfig::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SmpError {
    #[error("no such core: {0}")]
    NoSuchCore(CoreId),
    /// The `Smp` was shut down, before the work ran or while it ran.
    #[error("shut down")]
    Shutdown,
    /// The work panicked.
    #[error("the task panicked")]
    Panicked,
}

/// A running set of cores. Cheap to clone and usable from any thread.
#[derive(Clone)]
pub struct Smp {
    shared: Arc<Shared>,
}

struct Shared {
    cores: Vec<mpsc::Sender<Message>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    shut_down: AtomicBool,
}

enum Message {
    /// Spawn this on the core's `LocalSet`.
    Run(Box<dyn FnOnce() + Send>),
    /// Stop the core: its pending tasks are dropped.
    Shutdown,
}

thread_local! {
    static CURRENT: Cell<Option<CoreId>> = const { Cell::new(None) };
    /// This core's instances of every `Sharded` service, by service id.
    static LOCALS: RefCell<HashMap<u64, Rc<dyn Any>>> = RefCell::new(HashMap::new());
}

/// The core the calling thread is, if it's one.
pub fn current_core() -> Option<CoreId> {
    CURRENT.with(|c| c.get())
}

impl Smp {
    /// Starts the cores. Each runs until [`Smp::shutdown`].
    pub fn start(config: SmpConfig) -> std::io::Result<Smp> {
        let cpus = if config.pin {
            core_affinity::get_core_ids().unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut senders = Vec::new();
        let mut threads = Vec::new();
        for i in 0..config.cores.max(1) {
            let (tx, rx) = mpsc::channel(config.mailbox_capacity.max(1));
            let cpu = cpus.get(i).copied();
            let thread = std::thread::Builder::new()
                .name(format!("{}-{i}", config.name))
                .spawn(move || run_core(CoreId(i), cpu, rx))?;
            senders.push(tx);
            threads.push(thread);
        }
        Ok(Smp {
            shared: Arc::new(Shared {
                cores: senders,
                threads: Mutex::new(threads),
                shut_down: AtomicBool::new(false),
            }),
        })
    }

    pub fn cores(&self) -> usize {
        self.shared.cores.len()
    }

    pub fn core_ids(&self) -> impl Iterator<Item = CoreId> + use<> {
        (0..self.cores()).map(CoreId)
    }

    /// Runs `f` on `core` and returns what its future resolves to. The future
    /// runs on that core's thread and needn't be `Send`; `f` itself, and the
    /// result, cross threads.
    ///
    /// Waits for room in the core's mailbox. Work submitted from one thread
    /// to one core starts in submission order.
    pub async fn submit_to<F, Fut, T>(&self, core: CoreId, f: F) -> Result<T, SmpError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.send(
            core,
            Box::new(move || {
                tokio::task::spawn_local(async move {
                    let _ = tx.send(f().await);
                });
            }),
        )
        .await?;
        // A dropped sender means the task was dropped unfinished: a panic,
        // or the core stopped.
        rx.await.map_err(|_| {
            if self.shared.shut_down.load(Ordering::SeqCst) {
                SmpError::Shutdown
            } else {
                SmpError::Panicked
            }
        })
    }

    /// Runs `f` on `core` without waiting for it.
    pub async fn spawn_on<F, Fut>(&self, core: CoreId, f: F) -> Result<(), SmpError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.send(
            core,
            Box::new(move || {
                tokio::task::spawn_local(f());
            }),
        )
        .await
    }

    /// Runs `f` on every core and collects the results, in core order.
    pub async fn submit_to_all<F, Fut, T>(&self, f: F) -> Result<Vec<T>, SmpError>
    where
        F: Fn(CoreId) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let f = Arc::new(f);
        let calls = self.core_ids().map(|core| {
            let f = f.clone();
            self.submit_to(core, move || f(core))
        });
        let mut out = Vec::with_capacity(self.cores());
        for r in join_all(calls).await {
            out.push(r?);
        }
        Ok(out)
    }

    async fn send(&self, core: CoreId, task: Box<dyn FnOnce() + Send>) -> Result<(), SmpError> {
        if self.shared.shut_down.load(Ordering::SeqCst) {
            return Err(SmpError::Shutdown);
        }
        let mailbox = self
            .shared
            .cores
            .get(core.0)
            .ok_or(SmpError::NoSuchCore(core))?;
        mailbox
            .send(Message::Run(task))
            .await
            .map_err(|_| SmpError::Shutdown)
    }

    /// Stops every core, dropping the work still on them, and waits for
    /// their threads. Must not be called from a core, which would wait for
    /// itself.
    pub async fn shutdown(&self) {
        assert!(current_core().is_none(), "Smp::shutdown called from a core");
        if self.shared.shut_down.swap(true, Ordering::SeqCst) {
            return;
        }
        for mailbox in &self.shared.cores {
            let _ = mailbox.send(Message::Shutdown).await;
        }
        let threads = std::mem::take(&mut *self.shared.threads.lock().unwrap());
        let _ = tokio::task::spawn_blocking(move || {
            for t in threads {
                let _ = t.join();
            }
        })
        .await;
    }
}

fn run_core(core: CoreId, cpu: Option<core_affinity::CoreId>, mut rx: mpsc::Receiver<Message>) {
    if let Some(cpu) = cpu
        && !core_affinity::set_for_current(cpu)
    {
        tracing::debug!(%core, "couldn't pin the thread to a cpu");
    }
    CURRENT.with(|c| c.set(Some(core)));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("building a core's runtime");
    let local = LocalSet::new();
    local.block_on(&rt, async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                Message::Run(task) => task(),
                Message::Shutdown => break,
            }
        }
    });
    // Dropping the LocalSet drops the tasks still on it; then the services.
    drop(local);
    LOCALS.with(|l| l.borrow_mut().clear());
}

/// One instance of `T` per core, like Seastar's `sharded<T>`. The handle is
/// `Send` and `Sync`; each instance lives on its core and needn't be.
pub struct Sharded<T> {
    id: u64,
    smp: Smp,
    _t: PhantomData<fn() -> T>,
}

impl<T> Clone for Sharded<T> {
    fn clone(&self) -> Self {
        Sharded {
            id: self.id,
            smp: self.smp.clone(),
            _t: PhantomData,
        }
    }
}

static NEXT_SERVICE: AtomicU64 = AtomicU64::new(0);

impl<T: 'static> Sharded<T> {
    /// Builds an instance on every core with `make`, which runs on that core.
    pub async fn start<F, Fut>(smp: &Smp, make: F) -> Result<Sharded<T>, SmpError>
    where
        F: Fn(CoreId) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + 'static,
    {
        let id = NEXT_SERVICE.fetch_add(1, Ordering::Relaxed);
        let make = Arc::new(make);
        smp.submit_to_all(move |core| {
            let make = make.clone();
            async move {
                let instance: Rc<dyn Any> = Rc::new(make(core).await);
                LOCALS.with(|l| l.borrow_mut().insert(id, instance));
            }
        })
        .await?;
        Ok(Sharded {
            id,
            smp: smp.clone(),
            _t: PhantomData,
        })
    }

    /// This core's instance. Panics if the caller isn't on one of the
    /// service's cores, or the service was stopped.
    pub fn local(&self) -> Rc<T> {
        self.try_local()
            .expect("Sharded::local called off its cores, or after stop")
    }

    pub fn try_local(&self) -> Option<Rc<T>> {
        let any = LOCALS.with(|l| l.borrow().get(&self.id).cloned())?;
        Some(any.downcast::<T>().expect("service ids are unique"))
    }

    pub fn smp(&self) -> &Smp {
        &self.smp
    }

    /// Runs `f` with `core`'s instance, on that core.
    pub async fn invoke_on<F, Fut, R>(&self, core: CoreId, f: F) -> Result<R, SmpError>
    where
        F: FnOnce(Rc<T>) -> Fut + Send + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        let this = self.clone();
        self.smp
            .submit_to(core, move || async move { f(this.local()).await })
            .await
    }

    /// Runs `f` with every core's instance, collecting the results in core
    /// order.
    pub async fn invoke_on_all<F, Fut, R>(&self, f: F) -> Result<Vec<R>, SmpError>
    where
        F: Fn(Rc<T>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        let this = self.clone();
        self.smp.submit_to_all(move |_| f(this.local())).await
    }

    /// Drops every core's instance, on its core.
    pub async fn stop(&self) -> Result<(), SmpError> {
        let id = self.id;
        self.smp
            .submit_to_all(move |_| async move {
                let instance = LOCALS.with(|l| l.borrow_mut().remove(&id));
                drop(instance);
            })
            .await?;
        Ok(())
    }
}

/// Awaits every future, in order of the inputs. Small enough not to need the
/// `futures` crate.
async fn join_all<F: Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let handles: Vec<_> = futures.into_iter().map(Box::pin).collect();
    let mut out = Vec::with_capacity(handles.len());
    // Polling them all concurrently matters: each waits on a different core.
    let mut pending: Vec<_> = handles.into_iter().map(Some).collect();
    let mut results: Vec<Option<F::Output>> = (0..pending.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut done = true;
        for (slot, result) in pending.iter_mut().zip(results.iter_mut()) {
            if let Some(fut) = slot {
                match fut.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => {
                        *result = Some(v);
                        *slot = None;
                    }
                    std::task::Poll::Pending => done = false,
                }
            }
        }
        if done {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    out.extend(results.into_iter().map(|r| r.expect("all done")));
    out
}
