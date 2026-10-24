#![forbid(unsafe_code)]
use shep_smp_io2::{Buffer, Reactor};
use std::{
    future::Future,
    io::Read,
    pin::pin,
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

fn poll<F: Future>(f: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn futures_are_lazy_and_backpressure_returns_cancelled_waiters_buffers() {
    let (mut reactor, io) = Reactor::new(1).unwrap();
    let temp = tempfile::tempfile().unwrap();
    let file = io.file(temp);
    let first = file.write_at(Buffer::from_slice(b"first", 1).unwrap(), 0);
    let mut first = pin!(first);
    assert_eq!(io.in_flight(), 0);
    assert!(poll(first.as_mut()).is_pending());
    assert_eq!(io.in_flight(), 1);
    {
        let mut second = pin!(file.write_at(Buffer::from_slice(b"second", 1).unwrap(), 8));
        assert!(poll(second.as_mut()).is_pending());
        assert_eq!(io.in_flight(), 1);
    }
    reactor.shutdown().unwrap();
    let Poll::Ready((result, buffer)) = poll(first.as_mut()) else {
        panic!("completed write");
    };
    assert_eq!(result.unwrap(), 5);
    assert_eq!(&*buffer, b"first");
    assert_eq!(io.in_flight(), 0);
    let mut late = pin!(file.read_at(Buffer::new(4, 1).unwrap(), 0));
    let Poll::Ready((result, buffer)) = poll(late.as_mut()) else {
        panic!("closed admission");
    };
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(buffer.len(), 4);
}

#[test]
fn dropping_a_waiter_before_submission_keeps_accepted_work() {
    let (reactor, io) = Reactor::new(2).unwrap();
    let mut original = tempfile::tempfile().unwrap();
    let file = io.file(original.try_clone().unwrap());
    {
        let mut write = pin!(file.write_at(Buffer::from_slice(b"detached", 1).unwrap(), 0));
        assert!(poll(write.as_mut()).is_pending());
    }
    assert_eq!(io.in_flight(), 1);
    drop(reactor);
    let mut bytes = Vec::new();
    original.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"detached");
    assert_eq!(io.in_flight(), 0);
}

#[test]
fn submitted_waiter_can_be_dropped_and_remaining_operations_complete() {
    let (mut reactor, io) = Reactor::new(2).unwrap();
    let file = io.file(tempfile::tempfile().unwrap());
    {
        let mut write = pin!(file.write_at(Buffer::from_slice(b"submitted", 1).unwrap(), 0));
        assert!(poll(write.as_mut()).is_pending());
        reactor.poll().unwrap();
    }
    let mut sync = pin!(file.sync_all());
    assert!(poll(sync.as_mut()).is_pending());
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        reactor.poll().unwrap();
        if let Poll::Ready(result) = poll(sync.as_mut()) {
            result.unwrap();
            break;
        }
        assert!(Instant::now() < end);
        reactor.prepare_park().unwrap().wait(Some(end)).unwrap();
    }
    reactor.shutdown().unwrap();
}

#[test]
fn a_panicking_completion_waker_does_not_strand_other_completions() {
    struct Bad;
    impl std::task::Wake for Bad {
        fn wake(self: std::sync::Arc<Self>) {
            panic!("bad wake");
        }
    }
    let (mut reactor, io) = Reactor::new(2).unwrap();
    let file = io.file(tempfile::tempfile().unwrap());
    let mut first = pin!(file.write_at(Buffer::new(1, 1).unwrap(), 0));
    let mut second = pin!(file.write_at(Buffer::new(1, 1).unwrap(), 1));
    let bad = Waker::from(std::sync::Arc::new(Bad));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&bad))
            .is_pending()
    );
    assert!(poll(second.as_mut()).is_pending());
    reactor.shutdown().unwrap();
    assert!(poll(first.as_mut()).is_ready());
    assert!(poll(second.as_mut()).is_ready());
    assert_eq!(io.in_flight(), 0);
}

#[test]
fn completed_result_survives_dropping_the_reactor_owner() {
    let (reactor, io) = Reactor::new(1).unwrap();
    let file = io.file(tempfile::tempfile().unwrap());
    let mut write = pin!(file.write_at(Buffer::from_slice(b"survives", 1).unwrap(), 0));
    assert!(poll(write.as_mut()).is_pending());
    drop(reactor);
    let Poll::Ready((result, buffer)) = poll(write.as_mut()) else {
        panic!("retired result remains available");
    };
    assert_eq!(result.unwrap(), 8);
    assert_eq!(&*buffer, b"survives");
}
