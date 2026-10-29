//! Driver costs without executor/task allocations or an admission backlog.
#![forbid(unsafe_code)]
use criterion::{BenchmarkId, Criterion, Throughput};
use shep_smp2::io::{Buffer, Driver, Fd, Operation, WriteAt};
use std::{hint::black_box, time::Duration};

fn retire(driver: &mut Driver) {
    driver.submit().unwrap();
    while driver.in_flight() != 0 {
        driver.tick();
        if driver.in_flight() != 0 {
            driver.submit_and_wait(None).unwrap();
        }
    }
}

fn main() {
    let mut criterion = Criterion::default()
        .sample_size(40)
        .warm_up_time(Duration::from_millis(200))
        .measurement_time(Duration::from_secs(1))
        .configure_from_args();
    let mut group = criterion.benchmark_group("driver/nop_batch");
    for count in [1, 32, 256] {
        let mut driver = Driver::new(count).unwrap();
        let mut handles = Vec::with_capacity(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |bench, &count| {
                bench.iter(|| {
                    handles.extend((0..count).map(|_| driver.push(Operation::Nop).unwrap()));
                    retire(&mut driver);
                    for mut handle in handles.drain(..) {
                        black_box(handle.try_complete().unwrap().result.unwrap());
                    }
                });
            },
        );
        driver.drain().unwrap();
    }
    group.finish();
    let mut group = criterion.benchmark_group("driver/write_4k_batch");
    for count in [1, 32, 256] {
        let mut driver = Driver::new(count).unwrap();
        let file = std::rc::Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
        file.as_file().set_len((count * 4096) as u64).unwrap();
        let mut buffers: Vec<_> = (0..count)
            .map(|_| Buffer::new(4096, 4096).unwrap())
            .collect();
        let mut handles = Vec::with_capacity(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |bench, _| {
            bench.iter(|| {
                for (offset, buffer) in buffers.drain(..).enumerate() {
                    handles.push(
                        driver
                            .io()
                            .submit(WriteAt {
                                file: file.clone(),
                                buffer,
                                offset: (offset * 4096) as u64,
                            })
                            .unwrap(),
                    );
                }
                retire(&mut driver);
                for mut handle in handles.drain(..) {
                    let completion = handle.try_complete().unwrap();
                    assert_eq!(completion.result.unwrap().into_count(), 4096);
                    buffers.push(black_box(completion.operation.buffer));
                }
            });
        });
        driver.drain().unwrap();
    }
    group.finish();
    let mut driver = Driver::new(256).unwrap();
    criterion.bench_function("driver/ignored_nop_batch_256", |bench| {
        bench.iter(|| {
            for _ in 0..256 {
                driver.push(Operation::Nop).unwrap().release();
            }
            retire(&mut driver);
        })
    });
    let mut completed = driver.push(Operation::Nop).unwrap();
    retire(&mut driver);
    criterion.bench_function("driver/cancel_identity_pin_release", |bench| {
        bench.iter(|| {
            drop(black_box(completed.cancel_target()));
        })
    });
    completed.try_complete().unwrap().result.unwrap();
    driver.drain().unwrap();
    // Each iteration issues real cancellation SQEs against pending watches.
    // Retain the same descriptor and vectors so abandonment boxing is included
    // without per-iteration socket or vector creation.
    let mut group = criterion.benchmark_group("driver/cancel_batch_256");
    for abandon_percent in [0usize, 1, 10, 50, 100] {
        let mut driver = Driver::new(512).unwrap();
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let file = std::rc::Rc::new(Fd::new(socket).unwrap());
        let mut owners = Vec::with_capacity(256);
        group.throughput(Throughput::Elements(256));
        group.bench_with_input(
            BenchmarkId::from_parameter(abandon_percent),
            &abandon_percent,
            |bench, &percent| {
                bench.iter(|| {
                    for index in 0..256 {
                        let owner = driver
                            .push(Operation::PollAdd {
                                file: file.clone(),
                                events: libc::POLLIN as u32,
                            })
                            .unwrap();
                        let target = owner.cancel_target();
                        drop(driver.push(Operation::Cancel(target)).unwrap());
                        if index < 256 * percent / 100 {
                            drop(owner);
                        } else {
                            owners.push(owner);
                        }
                    }
                    retire(&mut driver);
                    for mut owner in owners.drain(..) {
                        assert_eq!(
                            owner
                                .try_complete()
                                .unwrap()
                                .result
                                .unwrap_err()
                                .raw_os_error(),
                            Some(libc::ECANCELED)
                        );
                    }
                })
            },
        );
        driver.drain().unwrap();
    }
    group.finish();
    criterion.final_summary();
}
