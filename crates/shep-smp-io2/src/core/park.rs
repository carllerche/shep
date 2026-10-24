//! The only direct syscalls used by the wakeup protocol.
use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    sync::{Arc, Weak},
    task::{Wake, Waker},
    thread::{self, ThreadId},
};

use super::park_state::ParkState;

pub(super) struct Event {
    file: File,
    state: ParkState,
    owner: ThreadId,
}
impl Event {
    pub(super) fn new() -> io::Result<Arc<Self>> {
        // SAFETY: eventfd takes no pointers; flags request a private nonblocking fd.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Successful eventfd returned a fresh owned descriptor, adopted once.
        Ok(Arc::new(Self {
            file: unsafe { File::from_raw_fd(fd) },
            state: ParkState::new(),
            owner: thread::current().id(),
        }))
    }
    pub(super) fn drain(&self) -> io::Result<()> {
        let mut bytes = [0; 8];
        // One eventfd read drains the whole counter (not semaphore mode). Do
        // not loop on successful reads: a wake storm must not starve the core.
        loop {
            match (&self.file).read(&mut bytes) {
                Ok(8) => return Ok(()),
                Ok(_) => return Err(io::Error::other("short eventfd read")),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
    pub(super) fn close(&self) {
        self.state.close();
    }
    pub(super) fn prepare(&self) -> io::Result<bool> {
        self.state
            .prepare()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "reactor closed"))
    }
    pub(super) fn notified(&self) -> bool {
        self.state.notified()
    }
    pub(super) fn finish(&self) {
        self.state.finish();
    }
    pub(super) fn fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
}

/// Thread-safe wake capability. Holds no strong reference to the reactor or fd.
/// An upgrade retains the descriptor for the entire write, preventing fd reuse
/// if destruction races with a foreign wake.
#[derive(Clone)]
pub struct WakeHandle(pub(super) Weak<Event>);
impl WakeHandle {
    /// Retain a wake token, including before the reactor enters its kernel wait.
    /// Returns false after the owning reactor has shut down.
    pub fn unpark(&self) -> io::Result<bool> {
        let Some(event) = self.0.upgrade() else {
            return Ok(false);
        };
        let Some(signal) = event.state.notify() else {
            return Ok(false);
        };
        // Owner-thread wakes never enter the eventfd write path, even during
        // prepare_park. Its NOTIFIED state makes the subsequent wait skip.
        if !signal || event.owner == thread::current().id() {
            return Ok(true);
        }
        loop {
            match (&event.file).write(&1u64.to_ne_bytes()) {
                Ok(8) => return Ok(true),
                Ok(_) => return Err(io::Error::other("short eventfd write")),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // Saturation already retains a readable wake token.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(e) => return Err(e),
            }
        }
    }
    pub fn waker(&self) -> Waker {
        Waker::from(Arc::new(self.clone()))
    }
}
impl Wake for WakeHandle {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        let _ = self.unpark();
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::core::{Reactor, WaitOutcome};
    use std::time::{Duration, Instant};
    fn counter(event: &Event) -> io::Result<u64> {
        let mut bytes = [0; 8];
        (&event.file).read_exact(&mut bytes)?;
        Ok(u64::from_ne_bytes(bytes))
    }
    fn assert_no_write(event: &Event) {
        assert_eq!(
            counter(event).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
    fn remote(wake: &WakeHandle) {
        let wake = wake.clone();
        thread::spawn(move || assert!(wake.unpark().unwrap()))
            .join()
            .unwrap();
    }
    #[test]
    fn owner_notifications_never_write_even_after_prepare() {
        let mut reactor = Reactor::new(1).unwrap();
        let event = reactor.event.as_ref().unwrap().clone();
        let wake = reactor.wake_handle();
        wake.unpark().unwrap();
        assert_no_write(&event);
        assert_eq!(
            reactor.prepare_park().unwrap().wait(None).unwrap(),
            WaitOutcome::Ready
        );
        let park = reactor.prepare_park().unwrap();
        wake.unpark().unwrap();
        assert_no_write(&event);
        assert_eq!(park.wait(None).unwrap(), WaitOutcome::Ready);
        assert_no_write(&event);
    }
    #[test]
    fn remote_running_notifications_skip_writes_and_survive_until_prepare() {
        let mut reactor = Reactor::new(1).unwrap();
        let event = reactor.event.as_ref().unwrap().clone();
        let wake = reactor.wake_handle();
        remote(&wake);
        assert_no_write(&event);
        assert_eq!(
            reactor.prepare_park().unwrap().wait(None).unwrap(),
            WaitOutcome::Ready
        );
        assert_no_write(&event);
    }
    #[test]
    fn remote_parked_notifications_coalesce_to_one_eventfd_write() {
        let mut reactor = Reactor::new(1).unwrap();
        let event = reactor.event.as_ref().unwrap().clone();
        let wake = reactor.wake_handle();
        let park = reactor.prepare_park().unwrap();
        thread::scope(|scope| {
            for _ in 0..4 {
                let wake = wake.clone();
                scope.spawn(move || {
                    for _ in 0..100 {
                        wake.unpark().unwrap();
                    }
                });
            }
        });
        assert_eq!(counter(&event).unwrap(), 1);
        assert_eq!(park.wait(None).unwrap(), WaitOutcome::Ready);
    }
    #[test]
    fn abandoned_ready_and_timed_out_waits_restore_running() {
        let mut reactor = Reactor::new(1).unwrap();
        let event = reactor.event.as_ref().unwrap().clone();
        let wake = reactor.wake_handle();
        drop(reactor.prepare_park().unwrap());
        remote(&wake);
        assert_no_write(&event);
        assert_eq!(
            reactor.prepare_park().unwrap().wait(None).unwrap(),
            WaitOutcome::Ready
        );
        remote(&wake);
        assert_no_write(&event);
        assert_eq!(
            reactor.prepare_park().unwrap().wait(None).unwrap(),
            WaitOutcome::Ready
        );
        assert_eq!(
            reactor
                .prepare_park()
                .unwrap()
                .wait(Some(Instant::now() + Duration::from_millis(2)))
                .unwrap(),
            WaitOutcome::TimedOut
        );
        remote(&wake);
        assert_no_write(&event);
    }
    #[test]
    fn a_delayed_foreign_notification_interrupts_a_kernel_wait() {
        let mut reactor = Reactor::new(1).unwrap();
        let event = reactor.event.as_ref().unwrap().clone();
        let wake = reactor.wake_handle();
        let park = reactor.prepare_park().unwrap();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(5));
            wake.unpark().unwrap();
        });
        assert_eq!(
            park.wait(Some(Instant::now() + Duration::from_secs(1)))
                .unwrap(),
            WaitOutcome::Ready
        );
        writer.join().unwrap();
        assert_eq!(counter(&event).unwrap(), 1);
    }
}
