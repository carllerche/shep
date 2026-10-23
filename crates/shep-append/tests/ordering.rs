//! Deterministic completion ordering and failures, independent of kernel timing.
use shep_append::{AppendFile, BufferPool, Config};
use shep_smp_io::{Buffer, FileIo};
use std::{
    cell::{Cell, RefCell},
    io,
    rc::Rc,
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Clone)]
struct Controlled(Rc<State>);
struct State {
    pending: RefCell<Vec<(u64, Buffer)>>,
    done: RefCell<Vec<u64>>,
    changed: Notify,
    syncs: Cell<usize>,
    fail: Cell<Option<u64>>,
    short: Cell<bool>,
    data: RefCell<Vec<u8>>,
    alignment: Cell<usize>,
    hold_sync: Cell<bool>,
    fail_sync: Cell<bool>,
}
impl Controlled {
    fn new() -> Self {
        Self(Rc::new(State {
            pending: RefCell::default(),
            done: RefCell::default(),
            changed: Notify::new(),
            syncs: Cell::new(0),
            fail: Cell::new(None),
            short: Cell::new(false),
            data: RefCell::default(),
            alignment: Cell::new(1),
            hold_sync: Cell::new(false),
            fail_sync: Cell::new(false),
        }))
    }
    fn release(&self, offset: u64) {
        self.0.done.borrow_mut().push(offset);
        self.0.changed.notify_waiters();
    }
}
impl FileIo for Controlled {
    fn alignment(&self) -> usize {
        self.0.alignment.get()
    }
    async fn read_at(&self, b: Buffer, _: u64) -> (io::Result<usize>, Buffer) {
        (Ok(0), b)
    }
    async fn write_at(&self, b: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        self.0.pending.borrow_mut().push((offset, b));
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let ready = self.0.done.borrow().iter().position(|p| *p == offset);
            if let Some(i) = ready {
                self.0.done.borrow_mut().remove(i);
                break;
            }
            changed.await;
        }
        let i = self
            .0
            .pending
            .borrow()
            .iter()
            .position(|(p, _)| *p == offset)
            .unwrap();
        let (_, b) = self.0.pending.borrow_mut().remove(i);
        if self.0.fail.get() == Some(offset) {
            return (Err(io::Error::other("injected write failure")), b);
        }
        let n = if self.0.short.get() {
            b.len().min(2)
        } else {
            b.len()
        };
        let mut data = self.0.data.borrow_mut();
        let len = data.len().max(offset as usize + n);
        data.resize(len, 0);
        data[offset as usize..offset as usize + n].copy_from_slice(&b[..n]);
        (Ok(n), b)
    }
    async fn sync_data(&self) -> io::Result<()> {
        self.0.syncs.set(self.0.syncs.get() + 1);
        while self.0.hold_sync.get() {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.0.hold_sync.get() {
                break;
            }
            changed.await;
        }
        if self.0.fail_sync.get() {
            return Err(io::Error::other("injected sync failure"));
        }
        Ok(())
    }
    async fn sync_all(&self) -> io::Result<()> {
        self.sync_data().await
    }
    async fn allocate(&self, _: u64, _: u64) -> io::Result<()> {
        Ok(())
    }
    async fn set_len(&self, _: u64) -> io::Result<()> {
        Ok(())
    }
    async fn len(&self) -> io::Result<u64> {
        Ok(100000)
    }
}
async fn opened(file: Controlled) -> AppendFile<Controlled> {
    AppendFile::open(
        file,
        BufferPool::new(32),
        Config {
            chunk_size: 4,
            preallocate: 0,
            idle: Duration::from_secs(3600),
        },
        0,
    )
    .await
    .unwrap()
}
async fn settle() {
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
}
#[tokio::test(flavor = "local")]
async fn out_of_order_writes_do_not_advance_the_contiguous_prefix_or_sync() {
    let f = Controlled::new();
    let a = opened(f.clone()).await;
    a.append(b"abcdefghijkl").await.unwrap();
    settle().await;
    let mut sync = std::pin::pin!(a.sync_through(12));
    assert!(shep_futures::poll!(&mut sync).is_pending());
    f.release(8);
    f.release(4);
    settle().await;
    assert_eq!(a.positions().written, 0);
    assert_eq!(f.0.syncs.get(), 0);
    f.release(0);
    sync.await.unwrap();
    assert_eq!(a.positions().written, 12);
    assert_eq!(a.positions().durable, 12);
    assert_eq!(f.0.syncs.get(), 1);
}
#[tokio::test(flavor = "local")]
async fn a_failed_earlier_write_cannot_be_hidden_by_later_successes() {
    let f = Controlled::new();
    f.0.fail.set(Some(0));
    let a = opened(f.clone()).await;
    a.append(b"abcdefgh").await.unwrap();
    settle().await;
    f.release(4);
    f.release(0);
    settle().await;
    assert_eq!(a.positions().written, 0);
    assert!(a.sync_through(8).await.is_err());
    assert!(a.append(b"x").await.is_err());
    assert_eq!(f.0.syncs.get(), 0);
}
#[tokio::test(flavor = "local")]
async fn short_writes_are_retried_at_the_remaining_offset() {
    let f = Controlled::new();
    f.0.short.set(true);
    let a = opened(f.clone()).await;
    a.append(b"abcd").await.unwrap();
    settle().await;
    f.release(0);
    f.release(2);
    a.sync_through(4).await.unwrap();
    assert_eq!(&*f.0.data.borrow(), b"abcd");
}
#[tokio::test(flavor = "local")]
async fn dropping_a_sync_waiter_does_not_cancel_persistence() {
    let f = Controlled::new();
    let a = opened(f.clone()).await;
    a.append(b"abcd").await.unwrap();
    settle().await;
    {
        let mut sync = std::pin::pin!(a.sync_through(4));
        assert!(shep_futures::poll!(&mut sync).is_pending());
    }
    f.release(0);
    a.sync_through(4).await.unwrap();
    assert_eq!(f.0.syncs.get(), 1);
}

#[tokio::test(flavor = "local")]
async fn partial_page_rewrites_wait_for_the_previous_write_and_its_sync() {
    let f = Controlled::new();
    f.0.alignment.set(4);
    f.0.hold_sync.set(true);
    let a = opened(f.clone()).await;
    a.append(b"a").await.unwrap();
    let mut sync = std::pin::pin!(a.sync_through(1));
    assert!(shep_futures::poll!(&mut sync).is_pending());
    settle().await;
    assert_eq!(&f.0.pending.borrow()[0].1[..], b"a\0\0\0");
    f.release(0);
    settle().await;
    assert_eq!(f.0.syncs.get(), 1);
    a.append(b"b").await.unwrap();
    a.dispatch();
    settle().await;
    assert!(
        f.0.pending.borrow().is_empty(),
        "a rewrite must not overlap the outstanding sync"
    );
    f.0.hold_sync.set(false);
    f.0.changed.notify_waiters();
    sync.await.unwrap();
    settle().await;
    assert_eq!(&f.0.pending.borrow()[0].1[..], b"ab\0\0");
    f.release(0);
    a.sync_through(2).await.unwrap();
    assert_eq!(&f.0.data.borrow()[..2], b"ab");
}

#[tokio::test(flavor = "local")]
async fn synchronization_failure_does_not_advance_durability() {
    let f = Controlled::new();
    let a = opened(f.clone()).await;
    a.append(b"abcd").await.unwrap();
    f.release(0);
    f.0.fail_sync.set(true);
    assert!(a.sync_through(4).await.is_err());
    assert_eq!(a.positions().written, 4);
    assert_eq!(a.positions().durable, 0);
    f.0.fail_sync.set(false);
    assert!(
        a.sync_through(4).await.is_err(),
        "the first failure must remain visible"
    );
    assert!(a.finish().await.is_err());
}
