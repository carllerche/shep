//! Bounded per-pair SPSC transport with unbounded, shard-local overflow.
//! External callers use a separate MPSC ingress. Publication wakes the peer
//! reactor directly; capacity is retried by ticks and bounded idle backoff.
use std::{
    cell::RefCell,
    collections::VecDeque,
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

use ringbuf::{HeapCons, HeapProd, HeapRb, traits::*};

use super::{Command, SmpError};
use crate::rt::{WakeHandle, context};

pub(super) const CAPACITY: usize = 128;
pub(super) const BATCH: usize = 16;
const BUDGET: usize = 64;

pub(super) struct Inbox {
    id: usize,
    external: mpsc::Sender<Command>,
    wake: OnceLock<WakeHandle>,
    failure: OnceLock<SmpError>,
    closed: AtomicBool,
}
impl Inbox {
    pub(super) fn new(id: usize) -> (Arc<Self>, mpsc::Receiver<Command>) {
        let (external, receiver) = mpsc::channel();
        (
            Arc::new(Self {
                id,
                external,
                wake: OnceLock::new(),
                failure: OnceLock::new(),
                closed: AtomicBool::new(false),
            }),
            receiver,
        )
    }
    fn notify(&self) {
        if let Some(wake) = self.wake.get() {
            let _ = wake.unpark();
        }
    }
    pub(super) fn error(&self) -> SmpError {
        self.failure.get().cloned().unwrap_or(SmpError::Shutdown)
    }
    fn closed(&self) -> bool {
        // Busy reactors do not enter the parking handshake. Observe closure
        // directly, without relying on a notification to acquire its publication.
        self.closed.load(Ordering::SeqCst)
    }
    pub(super) fn close(&self, error: SmpError) {
        self.failure.get_or_init(|| error);
        self.closed.store(true, Ordering::SeqCst);
        // The worker owns the receiver and drops it during teardown. A push
        // racing this close either fails or is cancelled by receiver destruction.
        self.notify();
    }
    pub(super) fn send(self: &Arc<Self>, command: Command) -> Result<(), SmpError> {
        if self.closed() {
            return Err(self.error());
        }
        if let Some(shard) = context::smp() {
            let belongs = shard
                .borrow()
                .inboxes
                .get(self.id)
                .is_some_and(|inbox| Arc::ptr_eq(inbox, self));
            if belongs {
                let to = self.id;
                let published = {
                    let mut shard = shard.borrow_mut();
                    // The owning thread may be executing task destructors during shutdown.
                    if shard.inboxes[shard.id].closed() {
                        return Err(SmpError::Shutdown);
                    }
                    if to == shard.id {
                        shard.local.push_back(command);
                        false
                    } else if let Some(out) = &mut shard.outgoing[to] {
                        out.pending.push_back(command);
                        let pending = out.pending.len();
                        // BATCH triggers publication; offer the entire backlog
                        // and leave anything that does not fit staged locally.
                        pending >= BATCH && out.flush(pending) != 0
                    } else {
                        return Err(self.error());
                    }
                };
                // This owner is running: its next runtime tick flushes partial
                // batches without scheduling a transport task or waking itself.
                if published {
                    self.notify();
                }
                return Ok(());
            }
        }
        self.external.send(command).map_err(|_| self.error())?;
        self.notify();
        Ok(())
    }
}

struct Outgoing {
    producer: HeapProd<Command>,
    pending: VecDeque<Command>,
}
impl Outgoing {
    fn flush(&mut self, limit: usize) -> usize {
        if self.pending.is_empty() {
            return 0;
        }
        // Never use drain here: an unconsumed drain would discard overflow.
        self.producer
            .push_iter(std::iter::from_fn(|| self.pending.pop_front()).take(limit))
    }
}

pub(crate) struct Shard {
    external: mpsc::Receiver<Command>,
    id: usize,
    inboxes: Vec<Arc<Inbox>>,
    outgoing: Vec<Option<Outgoing>>,
    incoming: Vec<Option<HeapCons<Command>>>,
    local: VecDeque<Command>,
    next: usize,
}
impl Shard {
    pub(super) fn mesh(inboxes: &[Arc<Inbox>], ingress: Vec<mpsc::Receiver<Command>>) -> Vec<Self> {
        let n = inboxes.len();
        let mut shards: Vec<_> = ingress
            .into_iter()
            .enumerate()
            .map(|(id, external)| Self {
                external,
                id,
                inboxes: inboxes.to_vec(),
                outgoing: (0..n).map(|_| None).collect(),
                incoming: (0..n).map(|_| None).collect(),
                local: VecDeque::new(),
                next: 0,
            })
            .collect();
        for from in 0..n {
            for to in 0..n {
                if from == to {
                    continue;
                }
                let (producer, consumer) = HeapRb::new(CAPACITY).split();
                shards[from].outgoing[to] = Some(Outgoing {
                    producer,
                    pending: VecDeque::new(),
                });
                shards[to].incoming[from] = Some(consumer);
            }
        }
        shards
    }

    /// Record the source only when it belongs to this SMP instance. CoreId alone
    /// is insufficient: different SMP groups reuse the same numeric IDs.
    pub(super) fn origin(destination: &Arc<Inbox>) -> Option<Arc<Inbox>> {
        let shard = context::smp()?;
        let shard = shard.borrow();
        shard
            .inboxes
            .get(destination.id)
            .is_some_and(|i| Arc::ptr_eq(i, destination))
            .then(|| shard.inboxes[shard.id].clone())
    }
}

/// Synchronous reactor poller. Scratch buffers are owned separately from Shard
/// so callbacks and captured-value destructors run without a transport borrow.
pub(crate) struct Pump {
    shard: Rc<RefCell<Shard>>,
    inbox: Arc<Inbox>,
    batch: Vec<Command>,
    notify: Vec<Arc<Inbox>>,
    retired: Vec<Outgoing>,
}
impl Pump {
    pub(crate) fn new(shard: Rc<RefCell<Shard>>, wake: WakeHandle) -> Self {
        let inbox = {
            let s = shard.borrow();
            s.inboxes[s.id].clone()
        };
        assert!(inbox.wake.set(wake).is_ok(), "SMP reactor already attached");
        let peers = shard.borrow().inboxes.len();
        Self {
            shard,
            inbox,
            batch: Vec::with_capacity(BUDGET),
            notify: Vec::with_capacity(peers),
            retired: Vec::new(),
        }
    }
    pub(crate) fn closed(&self) -> bool {
        self.inbox.closed()
    }
    pub(crate) fn pending_flush(&self) -> bool {
        self.shard
            .borrow()
            .outgoing
            .iter()
            .flatten()
            .any(|out| !out.pending.is_empty())
    }
    /// Returns true on useful progress. A full ring with staged work alone is
    /// not progress; the runtime bounds its next idle wait using pending_flush.
    pub(crate) fn tick(&mut self) -> bool {
        if self.closed() {
            return false;
        }
        let mut progress = false;
        {
            let mut shard = self.shard.borrow_mut();
            for to in 0..shard.inboxes.len() {
                if shard.inboxes[to].closed() {
                    if let Some(out) = shard.outgoing[to].take() {
                        self.retired.push(out);
                        progress = true;
                    }
                    continue;
                }
                if let Some(out) = &mut shard.outgoing[to]
                    && out.flush(BUDGET) != 0
                {
                    progress = true;
                    self.notify.push(shard.inboxes[to].clone());
                }
            }
            // Rotate among peers, self-submissions, and external ingress under
            // a total command budget, leaving room for application tasks.
            let sources = shard.incoming.len() + 2;
            for _ in 0..sources {
                if self.batch.len() == BUDGET {
                    break;
                }
                let source = shard.next;
                shard.next = (source + 1) % sources;
                let limit = BUDGET - self.batch.len();
                if source == sources - 1 {
                    for _ in 0..limit {
                        let Ok(command) = shard.external.try_recv() else {
                            break;
                        };
                        self.batch.push(command);
                    }
                } else if source == sources - 2 {
                    let count = limit.min(shard.local.len());
                    self.batch.extend(shard.local.drain(..count));
                } else if let Some(incoming) = &mut shard.incoming[source] {
                    self.batch.extend(incoming.pop_iter().take(limit));
                    // No capacity notification. The producer retries on its
                    // own ticks, with an exponential deadline when otherwise idle.
                }
            }
        }
        for endpoint in self.retired.drain(..) {
            drop(endpoint);
        }
        for peer in self.notify.drain(..) {
            peer.notify();
        }
        progress |= !self.batch.is_empty();
        for command in self.batch.drain(..) {
            command();
        }
        progress
    }
}
impl Drop for Shard {
    fn drop(&mut self) {
        let error = if std::thread::panicking() {
            SmpError::WorkerFailed(super::CoreId(self.id), "worker panicked".into())
        } else {
            SmpError::Shutdown
        };
        self.inboxes[self.id].close(error);
        // Producers must retire their endpoint and overflow after a failed peer
        // exits, even when they were waiting on a full ring. The last endpoint
        // drops any commands left in the ring and resolves their cancelled replies.
        for peer in &self.inboxes {
            peer.notify();
        }
    }
}

pub(super) struct Enter(PhantomData<Rc<()>>);
impl Enter {
    pub(super) fn new(shard: Rc<RefCell<Shard>>) -> Self {
        assert!(context::smp().is_none(), "SMP transport scopes cannot nest");
        context::set_smp(Some(shard));
        Self(PhantomData)
    }
}
impl Drop for Enter {
    fn drop(&mut self) {
        context::set_smp(None);
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::{io::WaitOutcome, rt::Driver};
    use std::{
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Instant,
    };

    fn mesh() -> (Vec<Arc<Inbox>>, Vec<Rc<RefCell<Shard>>>) {
        let (inboxes, ingress): (Vec<_>, Vec<_>) = (0..2).map(Inbox::new).unzip();
        let shards = Shard::mesh(&inboxes, ingress)
            .into_iter()
            .map(|s| Rc::new(RefCell::new(s)))
            .collect();
        (inboxes, shards)
    }
    fn tick(pump: &mut Pump) -> bool {
        let _enter = Enter::new(pump.shard.clone());
        pump.tick()
    }

    #[test]
    fn partial_batches_and_overflow_progress_on_ticks_without_capacity_notifications() {
        let (inboxes, shards) = mesh();
        let mut source_driver = Driver::new(8).unwrap().0;
        let destination_driver = Driver::new(8).unwrap().0;
        let mut source = Pump::new(shards[0].clone(), source_driver.wake_handle());
        let mut destination = Pump::new(shards[1].clone(), destination_driver.wake_handle());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let total = CAPACITY * 3 + 1;
        {
            let _enter = Enter::new(shards[0].clone());
            for i in 0..total {
                let seen = seen.clone();
                inboxes[1]
                    .send(Box::new(move || seen.lock().unwrap().push(i)))
                    .unwrap();
            }
        }
        assert_eq!(
            shards[0].borrow().outgoing[1]
                .as_ref()
                .unwrap()
                .pending
                .len(),
            total - CAPACITY
        );
        assert!(shards[1].borrow().external.try_recv().is_err());
        assert!(!tick(&mut source), "full rings do not count as progress");
        assert!(source.pending_flush());
        let parked = source_driver.prepare_park().unwrap();
        assert!(tick(&mut destination));
        assert_eq!(
            parked.wait(Some(Instant::now())).unwrap(),
            WaitOutcome::TimedOut,
            "draining must not notify the producer"
        );
        for _ in 0..total {
            tick(&mut source);
            tick(&mut destination);
            if seen.lock().unwrap().len() == total {
                break;
            }
        }
        assert_eq!(*seen.lock().unwrap(), (0..total).collect::<Vec<_>>());
        assert!(!source.pending_flush());
        let seen_next = seen.clone();
        {
            let _enter = Enter::new(shards[0].clone());
            inboxes[1]
                .send(Box::new(move || seen_next.lock().unwrap().push(total)))
                .unwrap();
        }
        assert!(shards[1].borrow().incoming[0].as_ref().unwrap().is_empty());
        assert!(tick(&mut source));
        assert!(tick(&mut destination));
        assert_eq!(seen.lock().unwrap().last(), Some(&total));
    }

    #[test]
    fn failed_consumer_releases_ring_and_overflow_and_notifies_parked_producer() {
        struct Probe(Arc<AtomicUsize>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let (inboxes, mut shards) = mesh();
        let mut driver = Driver::new(8).unwrap().0;
        let mut source = Pump::new(shards[0].clone(), driver.wake_handle());
        let dropped = Arc::new(AtomicUsize::new(0));
        {
            let _enter = Enter::new(shards[0].clone());
            for _ in 0..CAPACITY + BATCH {
                let probe = Probe(dropped.clone());
                inboxes[1].send(Box::new(move || drop(probe))).unwrap();
            }
        }
        assert!(!tick(&mut source));
        let park = driver.prepare_park().unwrap();
        let error = SmpError::WorkerFailed(super::super::CoreId(1), "test failure".into());
        inboxes[1].close(error.clone());
        drop(shards.pop());
        assert_eq!(park.wait(Some(Instant::now())).unwrap(), WaitOutcome::Ready);
        assert!(tick(&mut source));
        assert_eq!(dropped.load(Ordering::SeqCst), CAPACITY + BATCH);
        assert_eq!(inboxes[1].send(Box::new(|| {})), Err(error));
    }

    #[test]
    fn closing_external_ingress_drops_captures_without_borrowing_transport() {
        struct SubmitOnDrop(Arc<Inbox>);
        impl Drop for SubmitOnDrop {
            fn drop(&mut self) {
                assert_eq!(self.0.send(Box::new(|| {})), Err(SmpError::Shutdown));
            }
        }
        let (inbox, receiver) = Inbox::new(0);
        let capture = SubmitOnDrop(inbox.clone());
        inbox.send(Box::new(move || drop(capture))).unwrap();
        inbox.close(SmpError::Shutdown);
        drop(receiver);
    }
}
