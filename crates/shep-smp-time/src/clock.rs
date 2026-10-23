// Extracted from Monoio and adapted for shep-smp-time; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

//! Source of time abstraction.
//!
//! This extraction uses `std::time::Instant::now()`.

use crate::Instant;

#[derive(Default, Debug, Clone)]
pub(crate) struct Clock {}

pub(crate) fn now() -> Instant {
    Instant::from_std(std::time::Instant::now())
}

impl Clock {
    pub(crate) fn new() -> Clock {
        Clock {}
    }

    pub(crate) fn now(&self) -> Instant {
        now()
    }
}
