// Extracted from Monoio and adapted for Shep, commit 8675e9406ea7943c441d969e38c3f5fc198ef6d1.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

use std::{
    future::Future,
    marker::PhantomPinned,
    pin::Pin,
    task::{self, Poll},
};

use pin_project_lite::pin_project;

use crate::time::{Duration, Instant, entry::TimerEntry, timer::Handle};

// Preserve the existing overflow fallback: roughly 30 years from now.
fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(86400 * 365 * 30)
}

/// Waits until `deadline` is reached.
///
/// No work is performed while awaiting on the sleep future to complete. `Sleep`
/// operates at millisecond granularity and should not be used for tasks that
/// require high-resolution timers.
///
/// To run something regularly on a schedule, see [`interval`].
///
/// # Cancellation
///
/// Canceling a sleep instance is done by dropping the returned future. No
/// additional cleanup work is required.
///
/// # Examples
///
/// Wait 100ms and print "100 ms have elapsed".
///
/// ```
/// use shep_smp2::time::{sleep_until, Duration, Instant};
///
/// async fn example() {
///     sleep_until(Instant::now() + Duration::from_millis(100)).await;
///     println!("100 ms have elapsed");
/// }
/// ```
///
/// See the documentation for the [`Sleep`] type for more examples.
///
/// [`Sleep`]: struct@crate::time::Sleep
/// [`interval`]: crate::time::interval()
// Alias for old name in 0.x
#[cfg_attr(docsrs, doc(alias = "delay_until"))]
pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep::new_timeout(deadline)
}

/// Waits until `duration` has elapsed.
///
/// Equivalent to `sleep_until(Instant::now() + duration)`. An asynchronous
/// analog to `std::thread::sleep`.
///
/// No work is performed while awaiting on the sleep future to complete. `Sleep`
/// operates at millisecond granularity and should not be used for tasks that
/// require high-resolution timers.
///
/// To run something regularly on a schedule, see [`interval`].
///
/// The maximum duration for a sleep is 68719476734 milliseconds (approximately
/// 2.2 years).
///
/// # Cancellation
///
/// Canceling a sleep instance is done by dropping the returned future. No
/// additional cleanup work is required.
///
/// # Examples
///
/// Wait 100ms and print "100 ms have elapsed".
///
/// ```
/// use shep_smp2::time::{sleep, Duration};
///
/// async fn example() {
///     sleep(Duration::from_millis(100)).await;
///     println!("100 ms have elapsed");
/// }
/// ```
///
/// See the documentation for the [`Sleep`] type for more examples.
///
/// [`Sleep`]: struct@crate::time::Sleep
/// [`interval`]: crate::time::interval()
// Alias for old name in 0.x
#[cfg_attr(docsrs, doc(alias = "delay_for"))]
#[cfg_attr(docsrs, doc(alias = "wait"))]
pub fn sleep(duration: Duration) -> Sleep {
    match Instant::now().checked_add(duration) {
        Some(deadline) => sleep_until(deadline),
        None => sleep_until(far_future()),
    }
}

pin_project! {
    /// Future returned by [`sleep`](sleep) and [`sleep_until`](sleep_until).
    ///
    /// This type does not implement the `Unpin` trait, which means that if you
    /// use it in a selection combinator or by calling `poll`, you have to pin it first.
    /// If you use it with `.await`, this does not apply.
    ///
    /// # Examples
    ///
    /// Wait 100ms and print "100 ms have elapsed".
    ///
    /// ```
    /// use shep_smp2::time::{sleep, Duration};
    ///
    /// async fn example() {
    ///     sleep(Duration::from_millis(100)).await;
    ///     println!("100 ms have elapsed");
    /// }
    /// ```
    ///
    /// Pin the same `Sleep` to await and reset it repeatedly.
    /// ```no_run
    /// use shep_smp2::time::{self as time, Duration, Instant};
    ///
    /// async fn example() {
    ///     let sleep = time::sleep(Duration::from_millis(10));
    ///     let mut sleep = std::pin::pin!(sleep);
    ///
    ///     loop {
    ///         sleep.as_mut().await;
    ///         println!("timer elapsed");
    ///         sleep.as_mut().reset(Instant::now() + Duration::from_millis(50));
    ///     }
    /// }
    /// ```
    /// Use in a struct with boxing. By pinning the `Sleep` with a `Box`, the
    /// `HasSleep` struct implements `Unpin`, even though `Sleep` does not.
    /// ```
    /// use std::future::Future;
    /// use std::pin::Pin;
    /// use std::task::{Context, Poll};
    /// use shep_smp2::time::Sleep;
    ///
    /// struct HasSleep {
    ///     sleep: Pin<Box<Sleep>>,
    /// }
    ///
    /// impl Future for HasSleep {
    ///     type Output = ();
    ///
    ///     fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    ///         self.sleep.as_mut().poll(cx)
    ///     }
    /// }
    /// ```
    /// Use in a struct with pin projection. This method avoids the `Box`, but
    /// the `HasSleep` struct will not be `Unpin` as a consequence.
    /// ```
    /// use std::future::Future;
    /// use std::pin::Pin;
    /// use std::task::{Context, Poll};
    /// use shep_smp2::time::Sleep;
    /// use pin_project_lite::pin_project;
    ///
    /// pin_project! {
    ///     struct HasSleep {
    ///         #[pin]
    ///         sleep: Sleep,
    ///     }
    /// }
    ///
    /// impl Future for HasSleep {
    ///     type Output = ();
    ///
    ///     fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    ///         self.project().sleep.poll(cx)
    ///     }
    /// }
    /// ```
    ///
    /// [`select!`]: ../macro.select.html
    /// [`std::pin::pin!`]: ../macro.pin.html
    #[cfg_attr(docsrs, doc(alias = "Delay"))]
    #[derive(Debug)]
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub struct Sleep {
        state: SleepState,
        // Preserve the public pinning contract; the internal slab owner can move.
        #[pin]
        _pin: PhantomPinned,
    }
}

impl Sleep {
    pub(crate) fn new_timeout(deadline: Instant) -> Sleep {
        Sleep {
            state: SleepState::new(deadline),
            _pin: PhantomPinned,
        }
    }

    pub(crate) fn far_future() -> Sleep {
        Self::new_timeout(far_future())
    }

    /// Returns the instant at which the future will complete.
    pub fn deadline(&self) -> Instant {
        self.state.deadline()
    }

    /// Returns `true` if `Sleep` has elapsed.
    ///
    /// A `Sleep` instance is elapsed when the requested duration has elapsed.
    #[inline]
    pub fn is_elapsed(&self) -> bool {
        self.state.entry.is_elapsed()
    }

    /// Resets the `Sleep` instance to a new deadline.
    ///
    /// Calling this function allows changing the instant at which the `Sleep`
    /// future completes without having to create new associated state.
    ///
    /// This function can be called both before and after the future has
    /// completed.
    ///
    /// To call this method, you will usually combine the call with
    /// [`Pin::as_mut`], which lets you call the method without consuming the
    /// `Sleep` itself.
    ///
    /// # Example
    ///
    /// ```
    /// use shep_smp2::time::{Duration, Instant};
    ///
    /// # async fn example() {
    /// let sleep = shep_smp2::time::sleep(Duration::from_millis(10));
    /// let mut sleep = std::pin::pin!(sleep);
    ///
    /// sleep
    ///     .as_mut()
    ///     .reset(Instant::now() + Duration::from_millis(20));
    /// # }
    /// ```
    ///
    /// See also the top-level examples.
    ///
    /// [`Pin::as_mut`]: fn@std::pin::Pin::as_mut
    pub fn reset(self: Pin<&mut Self>, deadline: Instant) {
        self.project().state.reset(deadline);
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<()> {
        self.project().state.poll(cx)
    }
}

/// Address-independent sleep state shared by the pinned public future and intervals.
#[derive(Debug)]
pub(super) struct SleepState {
    deadline: Instant,
    entry: TimerEntry,
}

impl SleepState {
    pub(super) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            entry: TimerEntry::new(Handle::current()),
        }
    }

    pub(super) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(super) fn reset(&mut self, deadline: Instant) {
        self.entry.reset(deadline);
        self.deadline = deadline;
    }

    pub(super) fn poll(&mut self, cx: &mut task::Context<'_>) -> Poll<()> {
        match std::task::ready!(self.entry.poll_elapsed(self.deadline, cx)) {
            Ok(()) => Poll::Ready(()),
            Err(e) => panic!("timer error: {e}"),
        }
    }
}
