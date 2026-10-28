//! Public API benchmarks. Timing boundaries are documented in benches/task/README.md.
#![forbid(unsafe_code)]

use criterion::{BenchmarkId, Criterion, Throughput};
use shep_smp2::task::{Schedule, TaskId, new_task};
use std::cell::RefCell;
use std::future::{Future, pending, poll_fn};
use std::hint::black_box;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

#[derive(Default)]
struct Counter(AtomicUsize);
impl Schedule for Counter {
    fn schedule(&self, id: TaskId) {
        black_box(id);
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
fn poll<F: Future + Unpin>(f: &mut F, w: &Waker) -> Poll<F::Output> {
    Pin::new(f).poll(&mut Context::from_waker(w))
}
fn main() {
    let mut c = Criterion::default()
        .sample_size(60)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
        .configure_from_args();
    let scheduler = Arc::new(Counter::default());
    let mut group = c.benchmark_group("lifecycle");
    for count in [1, 64, 4096] {
        group.throughput(Throughput::Elements(count));
        for mode in [
            "join",
            "registered_join",
            "detach",
            "cancel",
            "cancel_polled",
        ] {
            group.bench_with_input(BenchmarkId::new(mode, count), &count, |b, &n| {
                b.iter(|| {
                    for _ in 0..n {
                        if mode.starts_with("cancel") {
                            let (mut task, mut join) =
                                new_task(pending::<usize>(), scheduler.clone());
                            if mode == "cancel_polled" {
                                assert!(!task.run());
                                assert!(poll(&mut join, Waker::noop()).is_pending());
                            }
                            drop(task);
                            assert!(poll(&mut join, Waker::noop()).is_ready());
                        } else {
                            let (mut task, mut join) =
                                new_task(async { black_box(42usize) }, scheduler.clone());
                            if mode == "registered_join" {
                                assert!(poll(&mut join, Waker::noop()).is_pending());
                            }
                            if mode == "detach" {
                                drop(join);
                                assert!(task.run());
                            } else {
                                assert!(task.run());
                                assert_eq!(poll(&mut join, Waker::noop()), Poll::Ready(Ok(42)));
                            }
                        }
                    }
                });
            });
        }
        // Keep a working set live to expose task cell/cache size effects.
        group.bench_with_input(BenchmarkId::new("resident", count), &count, |b, &n| {
            b.iter(|| {
                let mut tasks: Vec<_> = (0..n)
                    .map(|_| new_task(async { black_box([7u64; 16]) }, scheduler.clone()))
                    .collect();
                for (task, join) in &mut tasks {
                    assert!(task.run());
                    let _ = black_box(poll(join, Waker::noop()));
                }
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("steady");
    group.bench_function("self_wake_poll", |b| {
        let (mut task, _join) = new_task(
            poll_fn(|cx| {
                cx.waker().wake_by_ref();
                Poll::<()>::Pending
            }),
            scheduler.clone(),
        );
        b.iter(|| assert!(!task.run()));
    });
    group.bench_function("unnotified_run", |b| {
        let (mut task, _join) = new_task(pending::<()>(), scheduler.clone());
        assert!(!task.run());
        b.iter(|| assert!(!task.run()));
    });
    for replace in [false, true] {
        group.bench_function(if replace { "join_replace" } else { "join_same" }, |b| {
            let (_task, mut join) = new_task(pending::<()>(), scheduler.clone());
            let ws = [
                Waker::from(Arc::new(Counter::default())),
                Waker::from(Arc::new(Counter::default())),
            ];
            let mut index = 0;
            b.iter(|| {
                if replace {
                    index ^= 1;
                }
                assert!(poll(&mut join, &ws[index]).is_pending());
            });
        });
    }
    let saved = Rc::new(RefCell::new(None));
    let save = saved.clone();
    let (mut task, join) = new_task(
        poll_fn(move |cx| {
            *save.borrow_mut() = Some(cx.waker().clone());
            Poll::<()>::Pending
        }),
        scheduler.clone(),
    );
    assert!(!task.run());
    let waker: Waker = saved.borrow_mut().take().unwrap();
    group.bench_function("wake_and_poll", |b| {
        b.iter(|| {
            waker.wake_by_ref();
            assert!(!task.run());
        })
    });
    waker.wake_by_ref();
    group.bench_function("coalesced_wake", |b| b.iter(|| waker.wake_by_ref()));
    group.bench_function("clone_drop_waker", |b| {
        b.iter(|| drop(black_box(waker.clone())))
    });
    drop(task);
    drop(join);
    group.bench_function("completed_wake", |b| b.iter(|| waker.wake_by_ref()));
    group.finish();

    // One persistent producer. Thread startup is outside timing; channel
    // handoff, wake, and owner polling are included. No unsafe spin protocol.
    c.bench_function("cross_thread/wake_and_poll", |b| {
        let (send, recv) = mpsc::sync_channel::<Waker>(0);
        let (ack, done) = mpsc::sync_channel(0);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                while let Ok(waker) = recv.recv() {
                    waker.wake();
                    ack.send(()).unwrap();
                }
            });
            let saved = Rc::new(RefCell::new(None));
            let save = saved.clone();
            let (mut task, _join) = new_task(
                poll_fn(move |cx| {
                    *save.borrow_mut() = Some(cx.waker().clone());
                    Poll::<()>::Pending
                }),
                scheduler.clone(),
            );
            assert!(!task.run());
            let waker = saved.borrow_mut().take().unwrap();
            b.iter(|| {
                send.send(waker.clone()).unwrap();
                done.recv().unwrap();
                assert!(!task.run());
            });
            drop(send);
        });
    });
    c.final_summary();
}
