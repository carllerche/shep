//! Retained notification handshake, independent of file descriptors and threads.
#![forbid(unsafe_code)]
use std::sync::atomic::{AtomicU8, Ordering::SeqCst};

const RUNNING: u8 = 0;
const PARKING: u8 = 1;
const NOTIFIED: u8 = 2;
const CLOSED: u8 = 3;

// RMWs, including NOTIFIED -> NOTIFIED, linearize publication against the
// owner's prepare/finish transitions and final readiness check.
pub(super) struct ParkState(AtomicU8);
impl ParkState {
    pub(super) fn new() -> Self {
        Self(AtomicU8::new(RUNNING))
    }
    fn replace(&self, new: u8) -> Option<u8> {
        let mut current = self.0.load(SeqCst);
        loop {
            if current == CLOSED {
                return None;
            }
            match self.0.compare_exchange_weak(current, new, SeqCst, SeqCst) {
                Ok(previous) => return Some(previous),
                Err(actual) => current = actual,
            }
        }
    }
    /// Some(true) means a foreign caller must signal the eventfd. A local
    /// caller never writes: it cannot concurrently be blocked in the syscall.
    pub(super) fn notify(&self) -> Option<bool> {
        self.replace(NOTIFIED).map(|previous| previous == PARKING)
    }
    /// Publish parking before the final inbox check. A prior notification
    /// carries a permit that makes this wait attempt return without blocking.
    pub(super) fn prepare(&self) -> Option<bool> {
        let mut current = self.0.load(SeqCst);
        loop {
            if current == CLOSED {
                return None;
            }
            // A retained permit already makes this attempt skip the wait, so
            // don't advertise PARKING and provoke an unnecessary foreign write.
            let next = if current == NOTIFIED {
                RUNNING
            } else {
                PARKING
            };
            match self.0.compare_exchange_weak(current, next, SeqCst, SeqCst) {
                Ok(previous) => return Some(previous == NOTIFIED),
                Err(actual) => current = actual,
            }
        }
    }
    pub(super) fn notified(&self) -> bool {
        self.0.load(SeqCst) == NOTIFIED
    }
    pub(super) fn finish(&self) {
        self.replace(RUNNING);
    }
    pub(super) fn close(&self) {
        self.replace(CLOSED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn running_notifications_are_retained_and_parking_notifications_coalesce() {
        let state = ParkState::new();
        assert_eq!(state.notify(), Some(false));
        assert_eq!(state.prepare(), Some(true));
        // Consuming a prior permit doesn't advertise a new sleep attempt.
        assert_eq!(state.notify(), Some(false));
        state.finish();
        assert_eq!(state.prepare(), Some(false));
        assert_eq!(state.notify(), Some(true));
        assert_eq!(state.notify(), Some(false));
        assert!(state.notified());
        state.finish();
        state.close();
        assert_eq!(state.notify(), None);
        assert_eq!(state.prepare(), None);
        state.finish();
        assert_eq!(state.notify(), None);
    }
}
