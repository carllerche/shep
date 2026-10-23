use shep_futures::StreamExt;
use shep_futures::stream::{empty, unfold};

#[tokio::test(flavor = "local")]
async fn unfold_yields_until_none() {
    let stream = unfold(0u32, |n| async move {
        tokio::task::yield_now().await;
        (n < 3).then_some((n * 10, n + 1))
    });
    let mut stream = std::pin::pin!(stream);
    let mut out = Vec::new();
    while let Some(v) = stream.next().await {
        out.push(v);
    }
    assert_eq!(out, vec![0, 10, 20]);
    // Ended streams stay ended.
    assert_eq!(stream.next().await, None);
}

#[tokio::test(flavor = "local")]
async fn empty_ends_at_once() {
    let mut stream = empty::<u8>();
    assert_eq!(stream.next().await, None);
}
