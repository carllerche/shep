//! A runtime whose timer starts at a chosen epoch and is advanced by hand.
#![allow(dead_code)]

use std::ops::Deref;

use shep_smp2::time::{Instant, Timer};
use shep_smp2::{Config, Runtime};

pub struct ManualTimer(pub Runtime);

impl ManualTimer {
    pub fn new(epoch: Instant) -> Self {
        Self::from_timer(Timer::new(epoch))
    }

    pub fn with_capacity(epoch: Instant, capacity: usize) -> Self {
        Self::from_timer(Timer::with_capacity(epoch, capacity))
    }

    fn from_timer(timer: Timer) -> Self {
        Self(Runtime::build(Config::default(), Some(timer), |wake| wake).unwrap())
    }

    /// Run `f` with this runtime entered, so timer futures register here.
    pub fn with<R>(&self, f: impl FnOnce() -> R) -> R {
        let _enter = self.0.enter();
        f()
    }
}

impl Deref for ManualTimer {
    type Target = Timer;

    fn deref(&self) -> &Timer {
        self.0.timer()
    }
}
