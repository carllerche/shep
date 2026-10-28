//! Public-API benchmarks; see benches/time/README.md for timing boundaries.

#![forbid(unsafe_code)]

use std::future::Future;
use std::hint::black_box;
use std::mem::size_of;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Wake, Waker};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use shep_smp2::time::{Duration, Instant, Sleep, sleep_until};

#[path = "../../tests/support/timer.rs"]
mod support;
use support::ManualTimer;

const COUNTS: [usize; 3] = [1, 1024, 32_768];

#[derive(Clone, Copy)]
enum Deadlines {
    Dense,
    Spread,
    Levels,
    Later,
}

impl Deadlines {
    fn tick(self, index: usize) -> u64 {
        let index = index as u64;
        match self {
            Self::Dense => 4096,
            Self::Spread => 1 + (index * 37) % 4096,
            Self::Levels => {
                [1, 65, 4097, 262_145, 16_777_217, 1_073_741_825][index as usize % 6]
                    + (index / 6) % 63
            }
            Self::Later => 8192 + (index * 37) % 4096,
        }
    }
}

// Wake implements the real std Waker Arc lifecycle. The primary wheel-only
// cases use Waker::noop; the counted case makes wake delivery visible too.
#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

struct Timers {
    timer: ManualTimer,
    sleeps: Vec<Option<Pin<Box<Sleep>>>>,
    targets: Vec<Instant>,
    cancel_order: Vec<usize>,
    finish: Instant,
    waker: Waker,
}

impl Timers {
    fn new(count: usize, deadlines: Deadlines, waker: Waker) -> Self {
        let epoch = Instant::now();
        let timer = ManualTimer::new(epoch);
        let sleeps = timer.with(|| {
            (0..count)
                .map(|index| {
                    Some(Box::pin(sleep_until(
                        epoch + Duration::from_millis(deadlines.tick(index)),
                    )))
                })
                .collect()
        });
        let target_ticks: Vec<_> = (0..count)
            .map(|index| match deadlines {
                Deadlines::Later => 1 + (index as u64 * 37) % 64,
                _ => deadlines.tick(index) + 8192,
            })
            .collect();
        let finish_tick = (0..count)
            .map(|index| deadlines.tick(index).max(target_ticks[index]))
            .max()
            .unwrap();
        let targets = target_ticks
            .into_iter()
            .map(|tick| epoch + Duration::from_millis(tick))
            .collect();
        let mut cancel_order: Vec<_> = (0..count).collect();
        // Deterministic shuffle prepared off the clock. Avoid measuring only
        // the unusually cache-friendly insertion/reverse-insertion orders.
        let mut random = 42_u64;
        for index in (1..count).rev() {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            cancel_order.swap(index, (random >> 32) as usize % (index + 1));
        }
        Self {
            timer,
            sleeps,
            targets,
            cancel_order,
            finish: epoch + Duration::from_millis(finish_tick),
            waker,
        }
    }

    fn registered(count: usize, deadlines: Deadlines, waker: Waker) -> Self {
        let mut timers = Self::new(count, deadlines, waker);
        timers.register();
        timers
    }

    fn register(&mut self) {
        let mut cx = Context::from_waker(&self.waker);
        for sleep in self.sleeps.iter_mut().flatten() {
            let _ = black_box(sleep.as_mut().poll(&mut cx));
        }
    }

    fn cancel(&mut self) {
        for &index in &self.cancel_order {
            drop(self.sleeps[index].take());
        }
    }

    fn reset(&mut self) {
        for (sleep, &deadline) in self.sleeps.iter_mut().zip(&self.targets) {
            sleep.as_mut().unwrap().as_mut().reset(black_box(deadline));
        }
    }

    fn expire(&mut self) {
        self.timer.advance(black_box(self.finish));
    }
}

fn lifecycle(c: &mut Criterion) {
    let epoch = Instant::now();
    let deadline = epoch + Duration::from_secs(1);
    let mut group = c.benchmark_group("lifecycle");
    group.throughput(Throughput::Elements(1));
    group.bench_function("timer_create_drop", |b| {
        b.iter(|| drop(black_box(ManualTimer::new(black_box(epoch)))));
    });
    let timer = ManualTimer::new(epoch);
    timer.with(|| {
        group.bench_function("lazy_inline", |b| {
            b.iter(|| drop(black_box(sleep_until(black_box(deadline)))));
        });
        group.bench_function("register_cancel_inline", |b| {
            let mut cx = Context::from_waker(Waker::noop());
            b.iter(|| {
                let mut sleep = pin!(sleep_until(black_box(deadline)));
                let _ = black_box(sleep.as_mut().poll(&mut cx));
            });
        });
        group.bench_function("register_cancel_boxed", |b| {
            let mut cx = Context::from_waker(Waker::noop());
            b.iter(|| {
                let mut sleep = Box::pin(sleep_until(black_box(deadline)));
                let _ = black_box(sleep.as_mut().poll(&mut cx));
            });
        });
    });
    group.finish();
}

fn churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("churn");
    for count in COUNTS {
        group.throughput(Throughput::Elements(count as u64));
        let epoch = Instant::now();
        let timer = ManualTimer::new(epoch);
        let deadlines: Vec<_> = (0..count)
            .map(|index| epoch + Duration::from_millis(Deadlines::Spread.tick(index)))
            .collect();
        let mut sleeps = Vec::with_capacity(count);
        let mut cx = Context::from_waker(Waker::noop());
        timer.with(|| {
            group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
                b.iter(|| {
                    for &deadline in &deadlines {
                        let mut sleep = Box::pin(sleep_until(black_box(deadline)));
                        let _ = black_box(sleep.as_mut().poll(&mut cx));
                        sleeps.push(sleep);
                    }
                    for (sleep, &deadline) in sleeps.iter_mut().zip(&deadlines) {
                        sleep
                            .as_mut()
                            .reset(black_box(deadline + Duration::from_millis(8192)));
                    }
                    // Includes each Box allocation/free and all timer entry
                    // lifecycles, with a reused wheel and Vec backing storage.
                    sleeps.clear();
                });
            });
        });
    }
    group.finish();
}

fn operations(c: &mut Criterion) {
    // Setup and fixture destruction are excluded by iter_batched_ref. In the
    // cancel case, dropping each sleep is the operation, so its Box free counts.
    for (name, deadlines) in [
        ("register", Deadlines::Spread),
        ("cancel", Deadlines::Spread),
        ("reset_extend", Deadlines::Spread),
        ("reset_shorten", Deadlines::Later),
        ("expire_dense", Deadlines::Dense),
        ("expire_spread", Deadlines::Spread),
        ("expire_levels", Deadlines::Levels),
        ("expire_extended", Deadlines::Spread),
        ("expire_waker", Deadlines::Spread),
    ] {
        let mut group = c.benchmark_group(name);
        for count in COUNTS {
            group.throughput(Throughput::Elements(count as u64));
            // Select before entering the measured loop.
            let run: fn(&mut Timers) = match name {
                "register" => Timers::register,
                "cancel" => Timers::cancel,
                "reset_extend" | "reset_shorten" => Timers::reset,
                _ => Timers::expire,
            };
            group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
                b.iter_batched_ref(
                    || {
                        let waker = if name == "expire_waker" {
                            Waker::from(Arc::new(WakeCount::default()))
                        } else {
                            Waker::noop().clone()
                        };
                        let mut timers = Timers::new(count, deadlines, waker);
                        if name != "register" {
                            timers.register();
                        }
                        if name == "expire_extended" {
                            timers.reset();
                        }
                        timers
                    },
                    run,
                    // Each fixture owns a wheel and up to 32K pinned sleeps.
                    BatchSize::LargeInput,
                );
            });
        }
        group.finish();
    }
}

fn next_wake(c: &mut Criterion) {
    let mut group = c.benchmark_group("next_wake");
    group.throughput(Throughput::Elements(1));
    for count in COUNTS {
        let timers = Timers::registered(count, Deadlines::Levels, Waker::noop().clone());
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| black_box(timers.timer.next_wake()));
        });
    }
    group.finish();
}

fn pending(c: &mut Criterion) {
    for name in ["poll_pending", "is_elapsed"] {
        let mut group = c.benchmark_group(name);
        for count in COUNTS {
            group.throughput(Throughput::Elements(count as u64));
            let mut timers = Timers::registered(count, Deadlines::Spread, Waker::noop().clone());
            group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
                if name == "poll_pending" {
                    b.iter(|| timers.register());
                } else {
                    b.iter(|| {
                        for sleep in timers.sleeps.iter().flatten() {
                            black_box(black_box(sleep).is_elapsed());
                        }
                    });
                }
            });
        }
        group.finish();
    }
}

fn rearm(c: &mut Criterion) {
    let mut group = c.benchmark_group("rearm_elapsed");
    for count in COUNTS {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched_ref(
                || {
                    let mut timers =
                        Timers::registered(count, Deadlines::Spread, Waker::noop().clone());
                    timers.expire();
                    timers.register(); // Observe completion, like an interval's tick.
                    for (index, target) in timers.targets.iter_mut().enumerate() {
                        *target = timers.finish + Duration::from_millis(1 + index as u64 % 64);
                    }
                    timers
                },
                Timers::reset,
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

fn capacity_cases(c: &mut Criterion) {
    let mut group = c.benchmark_group("arena_registration");
    for capacity in [0, 256, 1024, 4096] {
        let counts = if capacity == 0 {
            vec![1]
        } else {
            vec![capacity, capacity + 1]
        };
        for count in counts {
            group.throughput(Throughput::Elements(count as u64));
            group.bench_function(BenchmarkId::new(capacity.to_string(), count), |b| {
                b.iter_batched_ref(
                    || {
                        let epoch = Instant::now();
                        let timer = ManualTimer::with_capacity(epoch, capacity);
                        let sleeps: Vec<_> = timer.with(|| {
                            (0..count)
                                .map(|_| Box::pin(sleep_until(epoch + Duration::from_millis(1))))
                                .collect()
                        });
                        (timer, sleeps)
                    },
                    |(_, sleeps)| {
                        let mut cx = Context::from_waker(Waker::noop());
                        for sleep in sleeps {
                            let _ = black_box(sleep.as_mut().poll(&mut cx));
                        }
                    },
                    BatchSize::LargeInput,
                );
            });
        }
    }
    group.finish();
}

struct Tasks {
    timer: ManualTimer,
    deadline: Instant,
}

impl Tasks {
    fn new(count: usize) -> Self {
        let epoch = Instant::now();
        let timer = ManualTimer::new(epoch);
        let deadline = epoch + Duration::from_millis(4096);
        for _ in 0..count {
            drop(timer.0.spawn(async move { sleep_until(deadline).await }));
        }
        timer.0.run_ready();
        Self { timer, deadline }
    }

    fn expire(&mut self) {
        self.timer.advance(black_box(self.deadline));
        black_box(self.timer.0.run_ready());
    }
}

fn executor(c: &mut Criterion) {
    let mut group = c.benchmark_group("executor_expire");
    for count in COUNTS {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched_ref(|| Tasks::new(count), Tasks::expire, BatchSize::LargeInput);
        });
    }
    group.finish();
}

// Run outside measurement, also under `cargo test --bench timers`. Assertions
// keep the benchmark from silently measuring completed/unregistered timers.
fn validate() {
    let count = 128;
    let counter = Arc::new(WakeCount::default());
    let mut timers = Timers::registered(count, Deadlines::Spread, Waker::from(counter.clone()));
    assert!(
        timers
            .sleeps
            .iter()
            .flatten()
            .all(|sleep| !sleep.is_elapsed())
    );
    timers.reset();
    timers.expire();
    assert_eq!(counter.0.load(Ordering::Relaxed), count);
    assert!(
        timers
            .sleeps
            .iter()
            .flatten()
            .all(|sleep| sleep.is_elapsed())
    );
    assert_eq!(timers.timer.next_wake(), None);
    let mut timers = Timers::registered(count, Deadlines::Later, Waker::noop().clone());
    timers.reset();
    timers.cancel();
    assert_eq!(timers.timer.next_wake(), None);
    let mut tasks = Tasks::new(count);
    assert!(!tasks.timer.0.is_empty());
    tasks.expire();
    assert!(tasks.timer.0.is_empty());
    assert_eq!(tasks.timer.next_wake(), None);
}

fn main() {
    eprintln!(
        "layout: Sleep={} bytes, Timer={} bytes",
        size_of::<Sleep>(),
        size_of::<shep_smp2::time::Timer>()
    );
    validate();
    let mut c = Criterion::default()
        .sample_size(50)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .configure_from_args();
    lifecycle(&mut c);
    operations(&mut c);
    churn(&mut c);
    next_wake(&mut c);
    pending(&mut c);
    executor(&mut c);
    rearm(&mut c);
    capacity_cases(&mut c);
    c.final_summary();
}
