//! Driver costs without executor/task allocations or an admission backlog.
#![forbid(unsafe_code)]
use criterion::{BenchmarkId, Criterion, Throughput};
use shep_smp_io2::{Buffer, Driver, Operation, Progress};
use std::{hint::black_box, sync::Arc, time::Duration};

fn retire(driver: &mut Driver) {
    let mut progress = Progress::default();
    driver.submit().unwrap();
    while driver.in_flight() != 0 {
        driver.reap_into(&mut progress);
        progress.wake();
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
                        black_box(handle.try_complete(driver.io()).unwrap().result.unwrap());
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
        let file = Arc::new(tempfile::tempfile().unwrap());
        file.set_len((count * 4096) as u64).unwrap();
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
                            .push(Operation::WriteAt {
                                file: file.clone(),
                                buffer,
                                offset: (offset * 4096) as u64,
                            })
                            .unwrap(),
                    );
                }
                retire(&mut driver);
                for mut handle in handles.drain(..) {
                    let completion = handle.try_complete(driver.io()).unwrap();
                    assert_eq!(completion.result.unwrap().into_count(), 4096);
                    buffers.push(black_box(completion.operation.into_buffer().unwrap()));
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
                driver.push(Operation::Nop).unwrap().release(driver.io());
            }
            retire(&mut driver);
        })
    });
    let mut completed = driver.push(Operation::Nop).unwrap();
    retire(&mut driver);
    criterion.bench_function("driver/cancel_identity_pin_release", |bench| {
        bench.iter(|| {
            drop(black_box(completed.cancel_target(driver.io())));
        })
    });
    completed.try_complete(driver.io()).unwrap().result.unwrap();
    driver.drain().unwrap();
    criterion.final_summary();
}
