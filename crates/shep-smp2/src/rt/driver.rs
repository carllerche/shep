//! The runtime's I/O driver and parking policy over ordinary PollAdd/Cancel operations.
#![forbid(unsafe_code)]
use super::event;
use crate::io::{Driver as IoDriver, Io, Operation, OperationHandle, Progress, WaitOutcome};
pub use event::WakeHandle;
use std::{io, sync::Arc, time::Instant};

pub struct Driver {
    driver: IoDriver,
    pub(super) timer: crate::time::Timer,
    progress: Progress,
    pub(super) event: Option<Arc<event::Event>>,
    watch: Option<OperationHandle>,
    event_fd: Option<std::rc::Rc<crate::io::Fd>>,
    closed: bool,
}
impl Driver {
    pub fn new(capacity: usize) -> io::Result<(Self, Io)> {
        if !(1..=32766).contains(&capacity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid data operation limit",
            ));
        }
        // The retained eventfd watch always occupies one ordinary operation.
        // Rearm before callbacks/tasks so that slot cannot be taken by data I/O.
        let driver = IoDriver::new(capacity + 1)?;
        let io = driver.io().clone();
        let event = event::Event::new()?;
        let event_fd = std::rc::Rc::new(crate::io::Fd::new(event.file().try_clone()?)?);
        let mut reactor = Self {
            driver,
            timer: crate::time::Timer::new(crate::time::Instant::now()),
            progress: Progress::default(),
            event: Some(event),
            event_fd: Some(event_fd),
            watch: None,
            closed: false,
        };
        reactor.arm()?;
        Ok((reactor, io))
    }
    fn arm(&mut self) -> io::Result<()> {
        if let Some(watch) = &mut self.watch {
            if let Some(completion) = watch.try_complete(self.driver.io()) {
                self.watch = None;
                completion.result?;
            } else {
                return Ok(());
            }
        }
        let event = self.event.as_ref().expect("open reactor");
        event.drain()?;
        self.watch = Some(
            self.driver
                .push(Operation::PollAdd {
                    file: self.event_fd.as_ref().unwrap().clone(),
                    events: libc::POLLIN as u32,
                })
                .map_err(|rejected| rejected.error)?,
        );
        Ok(())
    }
    pub fn wake_handle(&self) -> WakeHandle {
        WakeHandle(self.event.as_ref().map(Arc::downgrade).unwrap_or_default())
    }
    pub fn poll(&mut self) -> io::Result<usize> {
        match self.driver.submit() {
            Err(e) if transient(&e) => {}
            result => {
                result?;
            }
        }
        let retired = self.driver.reap_into(&mut self.progress);
        let armed = if retired != 0 { self.arm() } else { Ok(()) };
        self.progress.wake();
        armed?;
        Ok(retired)
    }
    pub fn prepare_park(&mut self) -> io::Result<Park<'_>> {
        if self.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        // new/poll keep the watch armed; only poll can retire its CQE.
        let event = self.event.as_ref().unwrap();
        event.drain()?;
        let notified = event.prepare()?;
        let park = Park {
            reactor: self,
            notified,
        };
        match park.reactor.driver.submit() {
            Err(e) if transient(&e) => {}
            result => {
                result?;
            }
        }
        Ok(park)
    }
    pub fn shutdown(&mut self) -> io::Result<()> {
        if self.event.is_none() {
            return Ok(());
        }
        self.closed = true;
        self.event.as_ref().unwrap().close();
        self.driver.io().close();
        // This cancellation uses the driver's reserved control slot even when
        // every ordinary slot is occupied. It also pins the watch identity.
        let cancellation = if let Some(watch) = &self.watch {
            let mut operation = Operation::Cancel(watch.cancel_target(self.driver.io()));
            loop {
                match self.driver.push(operation) {
                    Ok(handle) => break Some(handle),
                    Err(rejected) if rejected.error.kind() == io::ErrorKind::WouldBlock => {
                        // Shutdown synchronously retires an already accepted
                        // cancellation before using the single control reserve.
                        operation = rejected.operation;
                        match self.driver.submit() {
                            Err(e) if transient(&e) => {}
                            result => {
                                result?;
                            }
                        }
                        let retired = self.driver.reap_into(&mut self.progress);
                        self.progress.wake();
                        if retired == 0 {
                            self.driver.submit_and_wait(None)?;
                        }
                    }
                    Err(rejected) => return Err(rejected.error),
                }
            }
        } else {
            None
        };
        self.driver.drain()?;
        if let Some(mut cancellation) = cancellation {
            cancellation.release(self.driver.io());
        }
        if let Some(watch) = &mut self.watch {
            watch.release(self.driver.io());
        }
        self.watch = None;
        self.event = None;
        self.event_fd = None;
        Ok(())
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
#[must_use = "recheck ready work and deadlines before waiting"]
pub struct Park<'a> {
    reactor: &'a mut Driver,
    notified: bool,
}
impl Park<'_> {
    pub fn wait(self, deadline: Option<Instant>) -> io::Result<WaitOutcome> {
        if self.notified || self.reactor.event.as_ref().unwrap().notified() {
            return Ok(WaitOutcome::Ready);
        }
        self.reactor.driver.submit_and_wait(deadline)
    }
}
impl Drop for Park<'_> {
    fn drop(&mut self) {
        if let Some(event) = &self.reactor.event {
            event.finish();
        }
    }
}

fn transient(error: &io::Error) -> bool {
    error
        .raw_os_error()
        .is_some_and(|code| [libc::EINTR, libc::EAGAIN, libc::EBUSY].contains(&code))
}

/// Registration capabilities exposed through a runtime handle.
pub(crate) struct Handle {
    pub(crate) io: Io,
    pub(crate) timer: crate::time::Handle,
}

impl Driver {
    pub(super) fn handle(&self) -> Handle {
        Handle {
            io: self.driver.io().clone(),
            timer: self.timer.handle(),
        }
    }
}
