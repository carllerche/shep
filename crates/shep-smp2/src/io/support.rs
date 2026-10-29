//! Bounded blocking work owned by the I/O driver.
mod helper;
mod worker;
pub(crate) use worker::Worker;
fn wake_safely(wake: std::task::Waker) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wake.wake()));
}
