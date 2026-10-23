//! Thread-per-core, after Seastar's `smp` (design §3, §9.6).
//!
//! An [`Smp`] runs one OS thread per core, each with its own tokio
//! [`LocalRuntime`](tokio::runtime::LocalRuntime), which runs `!Send` tasks
//! spawned with `spawn_local` directly, and each with its own state, an `S`
//! built on that core when the cores start. Work on a core is ordinary async
//! code with no locks: the state is `Rc`/`RefCell`, and futures needn't be
//! `Send`. Cores talk only through [`Smp::submit_to`], which runs a closure
//! with another core's state, on that core, and returns its result.
//!
//! A single core is a complete deployment: everything runs, just on one
//! thread.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use shep_futures::future::join_all;
use tokio::runtime::{Handle, LocalOptions};
use tokio::sync::oneshot;

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
    /// A core's thread couldn't be started.
    #[error("starting a core: {0}")]
    Start(String),
}

/// A running set of cores, each with its state `S`. Cheap to clone and
/// usable from any thread; the states never leave their cores.
pub struct Smp<S> {
    shared: Arc<Shared>,
    _state: PhantomData<fn() -> S>,
}

impl<S> Clone for Smp<S> {
    fn clone(&self) -> Self {
        Smp {
            shared: self.shared.clone(),
            _state: PhantomData,
        }
    }
}

struct Shared {
    cores: Vec<Core>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    shut_down: AtomicBool,
}

/// How other threads reach a core.
struct Core {
    /// Spawns onto the core's runtime, from any thread.
    handle: Handle,
    /// Ends the core's runtime.
    stop: Mutex<Option<oneshot::Sender<()>>>,
}

thread_local! {
    static CURRENT: Cell<Option<CoreId>> = const { Cell::new(None) };
    /// This core's state. A core belongs to one `Smp`, so it has one, of
    /// that `Smp`'s `S`; it's stored erased because a static can't be
    /// generic.
    static STATE: RefCell<Option<Rc<dyn Any>>> = const { RefCell::new(None) };
}

/// The core the calling thread is, if it's one.
pub fn current_core() -> Option<CoreId> {
    CURRENT.with(|c| c.get())
}

/// The calling core's state.
fn state<S: 'static>() -> Rc<S> {
    let state = STATE.with(|s| s.borrow().clone());
    state
        .expect("a core's state is set before any call reaches it")
        .downcast::<S>()
        .expect("a core's state is its Smp's S")
}

impl<S: 'static> Smp<S> {
    /// Starts the cores, building each one's state with `make`, on that
    /// core. Each runs until [`Smp::shutdown`].
    pub async fn start<F, Fut>(config: SmpConfig, make: F) -> Result<Smp<S>, SmpError>
    where
        F: Fn(CoreId) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = S> + 'static,
    {
        let cpus = if config.pin {
            core_affinity::get_core_ids().unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut cores = Vec::new();
        let mut threads = Vec::new();
        for i in 0..config.cores.max(1) {
            let (stop, stopped) = oneshot::channel();
            let (handle_tx, handle_rx) = std::sync::mpsc::channel();
            let cpu = cpus.get(i).copied();
            let thread = std::thread::Builder::new()
                .name(format!("{}-{i}", config.name))
                .spawn(move || run_core(CoreId(i), cpu, handle_tx, stopped))
                .map_err(|e| SmpError::Start(e.to_string()))?;
            let handle = handle_rx
                .recv()
                .map_err(|_| SmpError::Start(format!("core {i} exited while starting")))?;
            cores.push(Core {
                handle,
                stop: Mutex::new(Some(stop)),
            });
            threads.push(thread);
        }
        let smp = Smp {
            shared: Arc::new(Shared {
                cores,
                threads: Mutex::new(threads),
                shut_down: AtomicBool::new(false),
            }),
            _state: PhantomData,
        };
        let make = Arc::new(make);
        let built = smp.core_ids().map(|core| {
            let make = make.clone();
            smp.run_on(core, move || async move {
                let state: Rc<dyn Any> = Rc::new(make(core).await);
                STATE.with(|s| *s.borrow_mut() = Some(state));
            })
        });
        for r in join_all(built).await {
            r?;
        }
        Ok(smp)
    }

    pub fn cores(&self) -> usize {
        self.shared.cores.len()
    }

    pub fn core_ids(&self) -> impl Iterator<Item = CoreId> + use<S> {
        (0..self.cores()).map(CoreId)
    }

    /// Runs `f` with `core`'s state, on that core, and returns what its
    /// future resolves to. The future needn't be `Send`; `f` itself, and the
    /// result, cross threads.
    ///
    /// The call is queued on the core when the returned future is first
    /// polled, and runs regardless after that. Work submitted from one thread
    /// to one core starts in submission order.
    ///
    /// The returned future holds its own handle, so not to wait for the
    /// result, spawn it. [`invoke_on_all`](Smp::invoke_on_all) reaches every
    /// core.
    pub fn submit_to<F, Fut, T>(
        &self,
        core: CoreId,
        f: F,
    ) -> impl Future<Output = Result<T, SmpError>> + use<F, Fut, T, S>
    where
        F: FnOnce(Rc<S>) -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        self.run_on(core, move || f(state::<S>()))
    }

    /// Runs `f` with every core's state, collecting the results in core
    /// order.
    pub async fn invoke_on_all<F, Fut, T>(&self, f: F) -> Result<Vec<T>, SmpError>
    where
        F: Fn(Rc<S>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let f = Arc::new(f);
        let calls = self.core_ids().map(|core| {
            let f = f.clone();
            self.submit_to(core, move |state| f(state))
        });
        join_all(calls).await.into_iter().collect()
    }

    /// Runs `f` on `core`: `submit_to`, without the state, which doesn't
    /// exist until `start` has built it.
    fn run_on<F, Fut, T>(
        &self,
        core: CoreId,
        f: F,
    ) -> impl Future<Output = Result<T, SmpError>> + use<F, Fut, T, S>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        // Owns a handle, so the call can be spawned.
        let shared = self.shared.clone();
        async move {
            if shared.shut_down.load(Ordering::SeqCst) {
                return Err(SmpError::Shutdown);
            }
            let target = shared.cores.get(core.0).ok_or(SmpError::NoSuchCore(core))?;
            // A `Send` task carries `f` to the core; there it builds the
            // future, which needn't be `Send`, and runs it as a local task.
            let call = target
                .handle
                .spawn(async move { tokio::task::spawn_local(f()).await });
            match call.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(e)) | Err(e) if e.is_panic() => Err(SmpError::Panicked),
                // Cancelled: the core's runtime stopped first.
                _ => Err(SmpError::Shutdown),
            }
        }
    }

    /// Stops every core, dropping the work still on them and then their
    /// states, and blocks the calling thread until their threads have
    /// exited.
    ///
    /// # Panics
    ///
    /// If called from a thread with a tokio runtime context: a runtime's own
    /// thread (a core's included), which it would block, but also tokio's
    /// blocking pool, which tokio gives no way to tell apart. From async code,
    /// call it on a plain thread of its own and await that.
    pub fn shutdown(&self) {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "Smp::shutdown blocks; call it from a thread with no tokio runtime"
        );
        if self.shared.shut_down.swap(true, Ordering::SeqCst) {
            return;
        }
        for core in &self.shared.cores {
            if let Some(stop) = core.stop.lock().unwrap().take() {
                let _ = stop.send(());
            }
        }
        let threads = std::mem::take(&mut *self.shared.threads.lock().unwrap());
        for t in threads {
            let _ = t.join();
        }
    }
}

fn run_core(
    core: CoreId,
    cpu: Option<core_affinity::CoreId>,
    handle: std::sync::mpsc::Sender<Handle>,
    stopped: oneshot::Receiver<()>,
) {
    if let Some(cpu) = cpu
        && !core_affinity::set_for_current(cpu)
    {
        tracing::debug!(%core, "couldn't pin the thread to a cpu");
    }
    CURRENT.with(|c| c.set(Some(core)));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(LocalOptions::default())
        .expect("building a core's runtime");
    let _ = handle.send(rt.handle().clone());
    // Work arrives as tasks spawned through the handle; the runtime runs
    // them until told to stop.
    rt.block_on(async move {
        let _ = stopped.await;
    });
    // Dropping the runtime drops the tasks still on it; then the state.
    drop(rt);
    STATE.with(|s| s.borrow_mut().take());
}
