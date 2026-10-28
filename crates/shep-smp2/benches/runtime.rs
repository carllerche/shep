//! Public API foundation benchmarks; see benches/README.md.
#![forbid(unsafe_code)]
use criterion::{BenchmarkId, Criterion, Throughput};
use shep_smp2::{Buffer, Config, File, Runtime, sleep, spawn};
use std::{future::poll_fn, hint::black_box, task::Poll, time::Duration};

fn main() {
    let mut c = Criterion::default()
        .sample_size(40)
        .warm_up_time(Duration::from_millis(200))
        .measurement_time(Duration::from_secs(1))
        .configure_from_args();
    let mut runtime = Runtime::new(Config::default()).unwrap();
    c.bench_function("runtime/ready_root", |b| {
        b.iter(|| black_box(runtime.block_on(async { black_box(42) }).unwrap()))
    });
    c.bench_function("runtime/self_wake_128_turns", |b| {
        b.iter(|| {
            runtime
                .block_on(async {
                    let mut left = 128;
                    poll_fn(move |cx| {
                        if left == 0 {
                            return Poll::Ready(());
                        }
                        left -= 1;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    })
                    .await;
                })
                .unwrap();
        })
    });
    c.bench_function("runtime/timer_idle_1ms", |b| {
        b.iter(|| {
            runtime
                .block_on(async { sleep(Duration::from_millis(1)).await })
                .unwrap()
        })
    });
    c.bench_function("runtime/foreign_handoff", |b| {
        let (send, recv) = std::sync::mpsc::sync_channel::<tokio::sync::oneshot::Sender<()>>(0);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                while let Ok(send) = recv.recv() {
                    let _ = send.send(());
                }
            });
            b.iter(|| {
                let (done, result) = tokio::sync::oneshot::channel();
                send.send(done).unwrap();
                runtime
                    .block_on(async move { result.await.unwrap() })
                    .unwrap();
            });
            drop(send);
        });
    });
    let original = tempfile::tempfile().unwrap();
    original.set_len(16 << 20).unwrap();
    let file = runtime.block_on(File::from_std(original)).unwrap().unwrap();
    let mut group = c.benchmark_group("io/write_read");
    for bytes in [64, 4096, 65536] {
        group.throughput(Throughput::Bytes((bytes * 2) as u64));
        let mut buffer = Some(Buffer::new(bytes, 4096).unwrap());
        group.bench_with_input(BenchmarkId::from_parameter(bytes), &bytes, |b, &bytes| {
            b.iter(|| {
                let buffer_in = buffer.take().unwrap();
                let file = file.clone();
                buffer = Some(
                    runtime
                        .block_on(async move {
                            let (result, buffer) = file.write_at(buffer_in, 0).await;
                            assert_eq!(result.unwrap(), bytes);
                            let (result, buffer) = file.read_at(buffer, 0).await;
                            assert_eq!(result.unwrap(), bytes);
                            black_box(buffer)
                        })
                        .unwrap(),
                );
            })
        });
    }
    group.finish();
    let mut group = c.benchmark_group("io/batch_write_4k");
    for count in [1, 32, 256] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let file = file.clone();
                runtime
                    .block_on(async move {
                        let mut joins = Vec::with_capacity(count as usize);
                        for index in 0..count {
                            let file = file.clone();
                            joins.push(spawn(async move {
                                let (result, _) = file
                                    .write_at(Buffer::new(4096, 4096).unwrap(), index * 4096)
                                    .await;
                                assert_eq!(result.unwrap(), 4096);
                            }));
                        }
                        for join in joins {
                            join.await.unwrap();
                        }
                    })
                    .unwrap();
            })
        });
    }
    group.finish();
    let (mut reactor, _io) = shep_smp2::rt::Driver::new(1).unwrap();
    let wake = reactor.wake_handle();
    c.bench_function("park/owner_notification", |b| {
        b.iter(|| {
            let park = reactor.prepare_park().unwrap();
            wake.unpark().unwrap();
            park.wait(None).unwrap();
            reactor.poll().unwrap();
        })
    });
    c.bench_function("park/foreign_running_burst_1024", |b| {
        let (send, recv) = std::sync::mpsc::sync_channel::<()>(0);
        let (ack, done) = std::sync::mpsc::sync_channel(0);
        let wake = wake.clone();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                while recv.recv().is_ok() {
                    for _ in 0..1024 {
                        wake.unpark().unwrap();
                    }
                    ack.send(()).unwrap();
                }
            });
            b.iter(|| {
                send.send(()).unwrap();
                done.recv().unwrap();
            });
            drop(send);
        });
    });
    c.bench_function("park/foreign_parking_handoff", |b| {
        let (send, recv) = std::sync::mpsc::sync_channel::<()>(0);
        let (ack, done) = std::sync::mpsc::sync_channel(0);
        let wake = wake.clone();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                while recv.recv().is_ok() {
                    wake.unpark().unwrap();
                    ack.send(()).unwrap();
                }
            });
            b.iter(|| {
                // Consume any prior permit before measuring a fresh parking
                // attempt (the preceding running-burst case leaves one).
                reactor
                    .prepare_park()
                    .unwrap()
                    .wait(Some(std::time::Instant::now()))
                    .unwrap();
                let park = reactor.prepare_park().unwrap();
                send.send(()).unwrap();
                park.wait(None).unwrap();
                done.recv().unwrap();
                reactor.poll().unwrap();
            });
            drop(send);
        });
    });
    runtime.shutdown().unwrap();
    c.final_summary();
}
