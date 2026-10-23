//! Compare the same checked operations and payloads with upstream slab.
#![forbid(unsafe_code)]

use std::hint::black_box;
use std::mem::size_of;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput};
use shep_util_slab::Slab as ShepSlab;
use slab::Slab as UpstreamSlab;

// A fixed payload exercises a slot size closer to runtime entries than u64.
type Payload = [u64; 8];

fn order(count: usize) -> Vec<usize> {
    let mut keys: Vec<_> = (0..count).collect();
    let mut random = 42_u64;
    for index in (1..count).rev() {
        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
        keys.swap(index, (random >> 32) as usize % (index + 1));
    }
    keys
}

// Instantiate identical harness code for each concrete slab, without adding
// a production abstraction or dynamic dispatch to the measured operations.
macro_rules! cases {
    ($c:ident, $slab:ident, $name:literal) => {
        for count in [1024, 32_768] {
            let mut group = $c.benchmark_group("slab_growth");
            group.throughput(Throughput::Elements(count as u64));
            group.bench_function(BenchmarkId::new($name, count), |b| {
                b.iter(|| {
                    let mut slab = $slab::new();
                    for index in 0..count {
                        black_box(slab.insert(black_box([index as u64; 8])));
                    }
                    // Construction, growth, and final destruction are timed.
                    black_box(slab)
                });
            });
            group.finish();

            let shuffled = order(count);
            let mut group = $c.benchmark_group("slab_churn");
            group.throughput(Throughput::Elements(count as u64));
            group.bench_function(BenchmarkId::new($name, count), |b| {
                let mut slab = $slab::with_capacity(count);
                let mut keys = Vec::with_capacity(count);
                b.iter(|| {
                    for index in 0..count {
                        keys.push(slab.insert(black_box([index as u64; 8])));
                    }
                    for &index in &shuffled {
                        black_box(slab.remove(keys[index]));
                    }
                    keys.clear();
                });
                assert!(slab.is_empty());
            });
            group.finish();

            let mut slab = $slab::with_capacity(count);
            for index in 0..count {
                slab.insert([index as u64; 8]);
            }
            let mut group = $c.benchmark_group("slab_get");
            group.throughput(Throughput::Elements(count as u64));
            group.bench_function(BenchmarkId::new($name, count), |b| {
                b.iter(|| {
                    for &key in &shuffled {
                        black_box(slab.get(black_box(key)).unwrap());
                    }
                });
            });
            group.finish();

            let mut group = $c.benchmark_group("slab_get_mut");
            group.throughput(Throughput::Elements(count as u64));
            group.bench_function(BenchmarkId::new($name, count), |b| {
                b.iter(|| {
                    for &key in &shuffled {
                        let value = slab.get_mut(black_box(key)).unwrap();
                        value[0] = value[0].wrapping_add(1);
                        black_box(value);
                    }
                });
            });
            group.finish();
        }
    };
}

fn main() {
    println!(
        "container layout: shep={} bytes, upstream={} bytes; payload={} bytes",
        size_of::<ShepSlab<Payload>>(),
        size_of::<UpstreamSlab<Payload>>(),
        size_of::<Payload>(),
    );
    let mut c = Criterion::default()
        .sample_size(50)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .configure_from_args();
    cases!(c, ShepSlab, "shep");
    cases!(c, UpstreamSlab, "upstream");
    c.final_summary();
}
