//! Retry deadlines for staged SMP commands when the reactor has no ready work.
use std::time::{Duration, Instant};

const INITIAL: Duration = Duration::from_micros(1);
const MAX: Duration = Duration::from_secs(1);

pub(super) struct FlushBackoff {
    delay: Duration,
    deadline: Option<Instant>,
}
impl Default for FlushBackoff {
    fn default() -> Self {
        Self {
            delay: INITIAL,
            deadline: None,
        }
    }
}
impl FlushBackoff {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Keep an existing deadline across early/spurious wakes. Only an expired
    /// retry advances the delay; useful work resets it in the runtime loop.
    pub(super) fn deadline(&mut self, now: Instant, pending: bool) -> Option<Instant> {
        if !pending {
            self.reset();
            return None;
        }
        match self.deadline {
            Some(deadline) if now < deadline => return Some(deadline),
            Some(_) => self.delay = (self.delay * 2).min(MAX),
            None => {}
        }
        let deadline = now + self.delay;
        self.deadline = Some(deadline);
        Some(deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_flush_deadlines_double_cap_and_reset_without_delaying_an_early_wake() {
        let mut backoff = FlushBackoff::default();
        let mut now = Instant::now();
        assert_eq!(backoff.deadline(now, false), None);
        let mut delay = INITIAL;
        for _ in 0..25 {
            let deadline = backoff.deadline(now, true).unwrap();
            assert_eq!(deadline.duration_since(now), delay);
            // Checking again, including on an early wake, must not extend it.
            assert_eq!(backoff.deadline(now, true), Some(deadline));
            assert_eq!(backoff.deadline(now + delay / 2, true), Some(deadline));
            now = deadline;
            delay = (delay * 2).min(MAX);
        }
        assert_eq!(delay, MAX);
        backoff.reset();
        assert_eq!(backoff.deadline(now, true), Some(now + INITIAL));
        assert_eq!(backoff.deadline(now, false), None);
        assert_eq!(backoff.deadline(now, true), Some(now + INITIAL));
    }
}
