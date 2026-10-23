//! Record-position bookkeeping above the reusable byte appender.
use crate::io::{File, WritePool};
use shep_append::{AppendFile, Config};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

/// Maps the active segment's byte progress to complete-record watermarks for the log.
pub(crate) struct Appender {
    /// Current segment, retained through a successful asynchronous close.
    active: Rc<RefCell<Option<Active>>>,
    /// Shared local chunk budget used by each activated byte appender.
    pool: WritePool,
    /// Buffering and preallocation policy copied into new segment appenders.
    config: AppenderConfig,
    /// Exclusive record watermark shared with the owning log.
    written: Arc<AtomicU64>,
}
/// Active byte appender and ordered record ends not yet reflected in the written watermark.
struct Active {
    /// Format-independent byte appender for the current segment.
    file: AppendFile<File>,
    /// Ordered (exclusive byte end, next record index) pairs awaiting write completion.
    records: VecDeque<(u64, u64)>,
    /// Next record index after all accepted records in this segment.
    end: u64,
}
/// The log's buffering policy translated to the format-independent byte appender.
pub(crate) struct AppenderConfig {
    /// Preferred chunk size in bytes before direct-I/O normalization.
    pub chunk_size: usize,
    /// Byte increment reserved ahead of writes; zero disables proactive reservation.
    pub prealloc_step: u64,
    /// Maximum idle delay before dispatching a partial chunk.
    pub idle: Duration,
}
/// Captured logical size and finish dependency; awaiting sync makes the segment durable.
pub(crate) struct Closed {
    /// Finish dependency that drains writes, trims padding, and synchronizes data.
    pub sync: shep_futures::future::LocalBoxFuture<'static, io::Result<()>>,
    /// Logical record bytes captured at close, excluding padding and preallocation.
    pub size: u64,
}
impl Appender {
    /// Create record-position bookkeeping with a shared byte pool and written watermark.
    pub(crate) fn new(pool: WritePool, config: AppenderConfig, written: Arc<AtomicU64>) -> Self {
        Self {
            active: Rc::new(RefCell::new(None)),
            pool,
            config,
            written,
        }
    }
    /// Attach a newly created segment at byte zero, with `next` as its first record index.
    pub(crate) async fn activate(&self, file: File, next: u64) -> io::Result<()> {
        let file = AppendFile::open(
            file,
            self.pool.clone(),
            Config {
                chunk_size: self.config.chunk_size,
                preallocate: self.config.prealloc_step,
                idle: self.config.idle,
            },
            0,
        )
        .await?;
        *self.active.borrow_mut() = Some(Active {
            file,
            records: VecDeque::new(),
            end: next,
        });
        Ok(())
    }
    /// Report whether a segment remains available for appends or an in-progress close.
    pub(crate) fn is_active(&self) -> bool {
        self.active.borrow().is_some()
    }
    /// Append a framed record and map its byte end to the next record index.
    /// The enclosing log transaction supplies exclusion and cancellation protection.
    pub(crate) async fn write(&self, header: &[u8], payload: &[u8], index: u64) -> io::Result<()> {
        let file = self.active.borrow().as_ref().unwrap().file.clone();
        file.append(header).await?;
        let end = file.append(payload).await?;
        {
            let mut active = self.active.borrow_mut();
            let active = active.as_mut().unwrap();
            active.records.push_back((end, index + 1));
            active.end = index + 1;
        }
        self.refresh_written();
        Ok(())
    }
    /// Advance the record watermark only past records whose entire byte range completed writing.
    pub(crate) fn refresh_written(&self) {
        let mut active = self.active.borrow_mut();
        let Some(active) = active.as_mut() else {
            return;
        };
        let written = active.file.positions().written;
        while active
            .records
            .front()
            .is_some_and(|(end, _)| *end <= written)
        {
            let (_, index) = active.records.pop_front().unwrap();
            self.written.fetch_max(index, Ordering::AcqRel);
        }
    }
    /// Capture the current byte and record ends for a shared log flush dependency.
    pub(crate) fn sync(&self) -> Option<(impl Future<Output = io::Result<()>> + use<>, u64)> {
        let active = self.active.borrow();
        let active = active.as_ref()?;
        let file = active.file.clone();
        let target = file.positions().appended;
        let end = active.end;
        Some((async move { file.sync_through(target).await }, end))
    }
    /// An explicit dependency for reads of an active segment.
    pub(crate) fn read_barrier(&self) -> impl Future<Output = io::Result<()>> + use<> {
        let active = self
            .active
            .borrow()
            .as_ref()
            .map(|a| (a.file.clone(), a.file.positions().appended));
        async move {
            if let Some((file, end)) = active {
                file.dispatch();
                file.wait_written(end).await?;
            }
            Ok(())
        }
    }
    /// Capture a finish operation that drains, trims, and synchronizes the active segment.
    /// Keep the active handle visible to read barriers until finishing succeeds.
    pub(crate) fn close(&self) -> Option<Closed> {
        let (file, end) = {
            let active = self.active.borrow();
            let active = active.as_ref()?;
            (active.file.clone(), active.end)
        };
        let active = self.active.clone();
        let size = file.positions().appended;
        let written = self.written.clone();
        Some(Closed {
            size,
            sync: Box::pin(async move {
                file.finish().await?;
                written.fetch_max(end, Ordering::AcqRel);
                active.borrow_mut().take();
                Ok(())
            }),
        })
    }
}
