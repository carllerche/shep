//! The runtime's I/O driver and parking policy over ordinary PollAdd/Cancel operations.
#![forbid(unsafe_code)]
use super::event;
use crate::io::{Driver as IoDriver, Io, Op, Operation, WaitOutcome};
pub use event::WakeHandle;
use std::{io, sync::Arc, time::Instant};

pub struct Driver {
    driver: IoDriver,
    pub(super) timer: crate::time::Timer,
    pub(super) event: Option<Arc<event::Event>>,
    watch: Option<Op<Operation>>,
    event_fd: Option<std::rc::Rc<crate::io::Fd>>,
    closed: bool,
}
impl Driver {
    pub fn new(submission_queue_capacity: usize) -> io::Result<(Self, Io)> {
        let driver = IoDriver::new(submission_queue_capacity)?;
        let io = driver.io().clone();
        let event = event::Event::new()?;
        let event_fd = std::rc::Rc::new(crate::io::Fd::new(event.file().try_clone()?)?);
        let mut reactor = Self {
            driver,
            timer: crate::time::Timer::new(crate::time::Instant::now()),
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
    /// Submit published work, deliver completions, rearm the wake watch and advance timers.
    pub fn tick(&mut self) -> io::Result<usize> {
        match self.driver.submit() {
            Err(e) if transient(&e) => {}
            result => {
                result?;
            }
        }
        let retired = self.driver.tick();
        // Publication can also reap the watch before this turn. Always check it.
        self.arm()?;
        self.timer.advance(crate::time::Instant::now());
        Ok(retired)
    }
    pub fn prepare_park(&mut self) -> io::Result<Park<'_>> {
        if self.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        // Publication may have reaped the watch since the last tick.
        self.arm()?;
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
    /// Park only after checking runnable work on both sides of the notification
    /// handshake. Arming can run completion callbacks and register new timers.
    pub(super) fn park_if_idle(&mut self, mut has_ready: impl FnMut() -> bool) -> io::Result<()> {
        self.timer.advance(crate::time::Instant::now());
        if has_ready() || self.driver.has_pending() {
            return Ok(());
        }
        let park = self.prepare_park()?;
        park.reactor.timer.advance(crate::time::Instant::now());
        let deadline = park.reactor.timer.next_wake();
        // Must follow PARKING: synchronize inbox publication or retain an
        // eventfd notification from a producer racing this final check.
        if has_ready() || park.reactor.driver.has_pending() {
            return Ok(());
        }
        park.wait(deadline)?;
        Ok(())
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        if self.event.is_none() {
            return Ok(());
        }
        self.closed = true;
        self.event.as_ref().unwrap().close();
        self.driver.io().close();
        // Cancellation is allowed after ordinary admission closes, and turns
        // over the SQ as needed. Its token pins the watch identity.
        let cancellation = if let Some(watch) = &self.watch {
            let mut operation = Operation::Cancel(watch.cancel_target(self.driver.io()));
            loop {
                match self.driver.push(operation) {
                    Ok(handle) => break Some(handle),
                    Err(rejected) if rejected.error.kind() == io::ErrorKind::WouldBlock => {
                        // Publication exhausted its bounded attempts. Shutdown
                        // continues driving accepted work before retrying.
                        operation = rejected.operation;
                        match self.driver.submit() {
                            Err(e) if transient(&e) => {}
                            result => {
                                result?;
                            }
                        }
                        let retired = self.driver.tick();
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

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn idle_parking_rechecks_ready_work_after_preparing() {
        let (mut reactor, _) = Driver::new(1).unwrap();
        reactor.tick().unwrap();
        let mut checks = 0;
        reactor
            .park_if_idle(|| {
                checks += 1;
                checks == 2
            })
            .unwrap();
        assert_eq!(checks, 2, "work appeared after the parking handshake");
        // Abandoning that parking attempt must restore the event state.
        drop(reactor.prepare_park().unwrap());
        reactor.shutdown().unwrap();
    }

    #[test]
    fn publication_reaping_the_watch_does_not_leave_parking_unarmed() {
        for poll_first in [false, true] {
            let (mut reactor, io) = Driver::new(1).unwrap();
            let wake = reactor.wake_handle();
            let park = reactor.prepare_park().unwrap();
            std::thread::spawn(move || wake.unpark().unwrap())
                .join()
                .unwrap();
            park.wait(None).unwrap();
            // Only the watch is outstanding, so its CQE is now available.
            reactor.driver.submit_and_wait(None).unwrap();
            let mut first = io.push(Operation::Nop).unwrap();
            let mut second = io.push(Operation::Nop).unwrap();
            // The full-SQ path reaped the watch outside reactor.tick().
            if poll_first {
                reactor.tick().unwrap();
            }
            drop(reactor.prepare_park().unwrap());
            assert!(
                reactor.watch.as_mut().unwrap().try_complete(&io).is_none(),
                "parking must replace the completed watch with an armed one"
            );
            first.release(&io);
            second.release(&io);
            reactor.shutdown().unwrap();
        }
    }
}
