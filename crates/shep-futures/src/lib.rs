//! The few future and stream utilities Shep uses, in place of `futures`.
//!
//! Everything here is for single-threaded runtimes: nothing is `Send` that
//! doesn't have to be. [`Stream`] is `futures-core`'s, so streams pass to and
//! from code built on `futures` unchanged.

pub mod future;
pub mod stream;

pub use future::FutureExt;
pub use futures_core::Stream;
pub use stream::StreamExt;

/// Polls a future once from inside an `async` block, returning its
/// [`Poll`](std::task::Poll) instead of waiting. The future must be `Unpin`;
/// pass a `Pin<&mut F>` for one that isn't.
#[macro_export]
macro_rules! poll {
    ($future:expr $(,)?) => {{
        let mut future = $future;
        ::std::future::poll_fn(|cx| {
            ::std::task::Poll::Ready(::std::future::Future::poll(
                ::std::pin::Pin::new(&mut future),
                cx,
            ))
        })
        .await
    }};
}
