use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use shep_futures::future::{block_on, join_all};
use shep_smp2::{CoreId, Smp, SmpConfig};

fn cross_shard(c: &mut Criterion) {
    let mut group = c.benchmark_group("smp/cross_shard_round_trips");
    for cores in [2, 4] {
        let smp = Smp::start(SmpConfig::with_cores(cores)).unwrap();
        for batch in [1, 16, 256, 2048] {
            group.throughput(Throughput::Elements((cores * batch) as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("{cores}_shards"), batch),
                &batch,
                |b, &batch| {
                    b.iter(|| {
                        let results = block_on(join_all(smp.core_ids().map(|source| {
                            let inner = smp.clone();
                            smp.submit_to(source, move || async move {
                                let destination = CoreId((source.0 + 1) % cores);
                                let replies = join_all((0..batch).map(|value| {
                                    inner.submit_to(destination, move || async move { value })
                                }))
                                .await;
                                for (expected, reply) in replies.into_iter().enumerate() {
                                    assert_eq!(reply.unwrap(), expected);
                                }
                            })
                        })));
                        for result in results {
                            result.unwrap();
                        }
                    });
                },
            );
        }
        smp.shutdown().unwrap();
    }
    group.finish();
}
criterion_group!(benches, cross_shard);
criterion_main!(benches);
