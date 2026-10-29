//! Exercise callback counts and reentrancy in the operation polling protocol.
#![cfg(not(miri))]
use shep_smp2::io::{Driver, Operation};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    task::{Context, RawWaker, RawWakerVTable, Waker},
};

thread_local! {
    static CLONES: Cell<usize> = const { Cell::new(0) };
    static ON_CLONE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}
fn callback_waker() -> Waker {
    fn clone(_: *const ()) -> RawWaker {
        CLONES.set(CLONES.get() + 1);
        let callback = ON_CLONE.with(|slot| slot.take());
        if let Some(callback) = callback {
            callback();
        }
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    fn noop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    // SAFETY: No callback dereferences the null data pointer or owns its storage.
    // Instrumentation is thread-local even if a waker crosses threads.
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

#[test]
fn unchanged_waiters_and_ready_results_do_not_clone_wakers() {
    let mut driver = Driver::new(1).unwrap();
    let mut op = driver.push(Operation::Nop).unwrap();
    let waker = callback_waker();
    let mut cx = Context::from_waker(&waker);
    CLONES.set(0);
    for _ in 0..1000 {
        assert!(op.poll_complete(&mut cx).is_pending());
    }
    assert_eq!(CLONES.get(), 1);
    driver.drain().unwrap();
    ON_CLONE
        .with(|slot| *slot.borrow_mut() = Some(Box::new(|| panic!("ready polls must not clone"))));
    assert!(op.poll_complete(&mut cx).is_ready());
    assert_eq!(CLONES.get(), 1);
    ON_CLONE.with(|slot| slot.take());
}

#[test]
fn clone_can_retire_the_request_before_waiter_registration() {
    let driver = Rc::new(RefCell::new(Driver::new(1).unwrap()));
    let mut op = driver.borrow().push(Operation::Nop).unwrap();
    let reenter = driver.clone();
    ON_CLONE.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || reenter.borrow_mut().drain().unwrap()))
    });
    let waker = callback_waker();
    assert!(
        op.poll_complete(&mut Context::from_waker(&waker))
            .is_ready()
    );
    assert_eq!(driver.borrow().in_flight(), 0);
}

#[test]
fn clone_panic_leaves_the_request_available_for_retry() {
    let mut driver = Driver::new(1).unwrap();
    let mut op = driver.push(Operation::Nop).unwrap();
    ON_CLONE.with(|slot| *slot.borrow_mut() = Some(Box::new(|| panic!("clone failed"))));
    let waker = callback_waker();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            op.poll_complete(&mut Context::from_waker(&waker))
        }))
        .is_err()
    );
    assert!(
        op.poll_complete(&mut Context::from_waker(&waker))
            .is_pending()
    );
    driver.drain().unwrap();
    assert!(
        op.poll_complete(&mut Context::from_waker(&waker))
            .is_ready()
    );
}
