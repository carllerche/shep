use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use shep_futures::FutureExt;
use shep_futures::future::{LocalBoxFuture, join_all};

#[tokio::test(flavor = "local")]
async fn join_all_keeps_the_given_order() {
    let finished = Rc::new(RefCell::new(Vec::new()));
    let calls = (0..5u64).map(|i| {
        let finished = finished.clone();
        async move {
            tokio::time::sleep(Duration::from_millis((5 - i) * 10)).await;
            finished.borrow_mut().push(i);
            i
        }
    });
    assert_eq!(join_all(calls).await, vec![0, 1, 2, 3, 4]);
    // They ran at once: the shortest sleep finished first.
    assert_eq!(*finished.borrow(), vec![4, 3, 2, 1, 0]);
}

#[tokio::test(flavor = "local")]
async fn join_all_of_nothing() {
    let none: Vec<std::future::Ready<u8>> = Vec::new();
    assert!(join_all(none).await.is_empty());
}

#[tokio::test(flavor = "local")]
async fn catch_unwind_returns_the_panic() {
    let ok = async { 1 }.catch_unwind().await;
    assert_eq!(ok.unwrap(), 1);
    let err = std::panic::AssertUnwindSafe(async {
        tokio::task::yield_now().await;
        panic!("handler")
    })
    .catch_unwind()
    .await
    .unwrap_err();
    assert_eq!(err.downcast_ref::<&str>(), Some(&"handler"));
}

#[tokio::test(flavor = "local")]
async fn boxed_local_holds_non_send_futures() {
    let rc = Rc::new(3);
    let boxed: LocalBoxFuture<'_, i32> = async move { *rc }.boxed_local();
    assert_eq!(boxed.await, 3);
}

#[tokio::test(flavor = "local")]
async fn poll_polls_once() {
    let (tx, rx) = tokio::sync::oneshot::channel::<u8>();
    let mut rx = std::pin::pin!(rx);
    assert!(shep_futures::poll!(rx.as_mut()).is_pending());
    tx.send(1).unwrap();
    assert_eq!(
        shep_futures::poll!(rx.as_mut()),
        std::task::Poll::Ready(Ok(1))
    );
}
