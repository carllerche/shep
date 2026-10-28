// Adapted from Monoio's task/harness.rs; see docs/task/FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

use std::{
    future::Future,
    panic::{self, AssertUnwindSafe},
    ptr::NonNull,
    task::{Context, Poll, Waker},
};

use crate::task::{
    JoinError,
    core::{Cell, Core, CoreStage, Header, Trailer},
    state::{Snapshot, TransitionToIdle},
};

pub(crate) struct Harness<T: Future> {
    cell: NonNull<Cell<T>>,
}

impl<T: Future> Harness<T> {
    /// The allocation must contain a Cell<T> and remain alive for this call.
    pub(crate) unsafe fn from_raw(ptr: NonNull<Header>) -> Self {
        Self { cell: ptr.cast() }
    }

    fn cell(&self) -> &Cell<T> {
        // SAFETY: from_raw's caller supplies the matching vtable type and
        // retains a local owner until this operation finishes.
        unsafe { self.cell.as_ref() }
    }

    fn header(&self) -> &Header {
        &self.cell().header
    }

    fn trailer(&self) -> &Trailer {
        &self.cell().trailer
    }

    fn core(&self) -> &Core<T> {
        &self.cell().core
    }

    /// Called only by the owner's unique Task handle. Shared wakers update
    /// notification bits but never touch the local stage or trailer.
    pub(super) fn poll(self) {
        self.header().shared.state.transition_to_running();
        let waker = Waker::from(self.header().shared.clone());
        let cx = Context::from_waker(&waker);
        if poll_future(&self.core().stage, cx).is_ready() {
            self.complete();
        } else if let TransitionToIdle::OkNotified = self.header().shared.state.transition_to_idle()
        {
            self.header()
                .shared
                .scheduler
                .schedule(self.header().shared.id);
        }
    }

    pub(super) fn cancel(self) {
        if self.header().shared.state.transition_to_cancelled() {
            // Drop the future in place. Stage assignment installs the error
            // even if its destructor unwinds, so a joiner is still notified.
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                self.core().stage.store_output(Err(JoinError::Cancelled));
            }));
            self.complete();
        }
    }

    pub(super) fn dealloc(self) {
        // SAFETY: The last of the two local owners releases the original Box.
        // Wakers own only Notification; no local cell access follows this call.
        unsafe { drop(Box::from_raw(self.cell.as_ptr())) }
    }

    pub(super) fn try_read_output(
        self,
        dst: &mut Poll<Result<T::Output, JoinError>>,
        waker: &Waker,
    ) {
        if can_read_output(self.header(), self.trailer(), waker) {
            *dst = Poll::Ready(self.core().stage.take_output());
        }
    }

    pub(super) fn drop_join_handle_slow(self) {
        let mut maybe_panic = None;
        if self.header().shared.state.unset_join_interested().is_err() {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                self.core().stage.drop_future_or_output();
            }));
            if let Err(panic) = result {
                maybe_panic = Some(panic);
            }
        } else {
            // There can be no concurrent local join poll. Release any old
            // joiner's waker immediately, even while the child is pending.
            drop(self.trailer().take_waker());
        }
        self.drop_reference();
        if let Some(panic) = maybe_panic {
            panic::resume_unwind(panic);
        }
    }

    fn drop_reference(self) {
        if self.header().shared.state.ref_dec() {
            self.dealloc();
        }
    }

    /// Complete a running task and release its joiner's waker. The owner's
    /// Task handle keeps the cell alive even if a wake drops the join handle.
    fn complete(&self) {
        let snapshot = self.header().shared.state.transition_to_complete();
        let waker = self.trailer().take_waker();
        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            if !snapshot.is_join_interested() {
                self.core().stage.drop_future_or_output();
            }
        }));
        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            if let Some(waker) = waker {
                waker.wake();
            }
        }));
    }
}

fn can_read_output(header: &Header, trailer: &Trailer, waker: &Waker) -> bool {
    // Load a snapshot of the current task state
    let snapshot = header.shared.state.load();

    debug_assert!(snapshot.is_join_interested());

    if !snapshot.is_complete() {
        // The waker must be stored in the task struct.
        let res = if snapshot.has_join_waker() {
            // There already is a waker stored in the struct. If it matches
            // the provided waker, then there is no further work to do.
            // Otherwise, the waker must be swapped.
            let will_wake = trailer.will_wake(waker);

            if will_wake {
                // The task is not complete **and** the waker is up to date,
                // there is nothing further that needs to be done.
                return false;
            }

            // Unset the `JOIN_WAKER` to gain mutable access to the `waker`
            // field then update the field with the new join worker.
            //
            // This requires two atomic operations, unsetting the bit and
            // then resetting it. If the task transitions to complete
            // concurrently to either one of those operations, then setting
            // the join waker fails and we proceed to reading the task
            // output.
            header
                .shared
                .state
                .unset_waker()
                .and_then(|snapshot| set_join_waker(header, trailer, waker.clone(), snapshot))
        } else {
            set_join_waker(header, trailer, waker.clone(), snapshot)
        };

        match res {
            Ok(_) => return false,
            Err(snapshot) => {
                assert!(snapshot.is_complete());
            }
        }
    }
    true
}

fn set_join_waker(
    header: &Header,
    trailer: &Trailer,
    waker: Waker,
    snapshot: Snapshot,
) -> Result<Snapshot, Snapshot> {
    assert!(snapshot.is_join_interested());
    assert!(!snapshot.has_join_waker());

    trailer.set_waker(waker);

    // Update the `JoinWaker` state accordingly
    let res = header.shared.state.set_join_waker();

    // If the state could not be updated, then clear the join waker
    if res.is_err() {
        drop(trailer.take_waker());
    }

    res
}

/// Poll the future and turn unwinding into an ordinary task failure.
fn poll_future<T: Future>(core: &CoreStage<T>, cx: Context<'_>) -> Poll<()> {
    let output = match panic::catch_unwind(AssertUnwindSafe(|| core.poll(cx))) {
        Ok(Poll::Pending) => return Poll::Pending,
        Ok(Poll::Ready(output)) => Ok(output),
        Err(_) => {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| core.drop_future_or_output()));
            Err(JoinError::Panicked)
        }
    };
    core.store_output(output);
    Poll::Ready(())
}
