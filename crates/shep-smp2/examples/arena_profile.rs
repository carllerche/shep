//! Fixed-work attribution probe; see benches/time/ATTRIBUTION.md.
#![forbid(unsafe_code)]

use std::future::Future;
use std::hint::black_box;
use std::io::{self, Write};
use std::pin::Pin;
use std::task::{Context, Waker};

use shep_smp2::time::{Duration, Instant, Sleep, Timer, sleep_until};

struct Fixture {
    timer: Timer,
    sleeps: Vec<Option<Pin<Box<Sleep>>>>,
}

impl Fixture {
    fn new(count: usize, capacity: usize) -> Self {
        let epoch = Instant::now();
        let timer = Timer::with_capacity(epoch, capacity);
        let sleeps = timer.with(|| {
            (0..count)
                .map(|index| {
                    Some(Box::pin(sleep_until(
                        epoch + Duration::from_millis(1 + (index as u64 * 37) % 4096),
                    )))
                })
                .collect()
        });
        Self { timer, sleeps }
    }

    fn poll(&mut self) {
        let mut cx = Context::from_waker(Waker::noop());
        for sleep in self.sleeps.iter_mut().flatten() {
            let _ = black_box(sleep.as_mut().poll(&mut cx));
        }
    }
}

#[inline(never)]
fn profile_readiness(fixture: &Fixture, rounds: usize) {
    for _ in 0..rounds {
        for sleep in fixture.sleeps.iter().flatten() {
            black_box(black_box(sleep).is_elapsed());
        }
    }
}

#[inline(never)]
fn profile_pending(fixture: &mut Fixture, rounds: usize) {
    for _ in 0..rounds {
        fixture.poll();
    }
}

#[inline(never)]
fn profile_register(fixtures: &mut [Fixture]) {
    for fixture in fixtures {
        fixture.poll();
    }
}

fn handshake(message: &str, enabled: bool) {
    if enabled {
        println!("{message}");
        io::stdout().flush().unwrap();
        let mut line = String::new();
        assert_ne!(io::stdin().read_line(&mut line).unwrap(), 0);
    }
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("readiness");
    let number = |index: usize, default: usize| {
        args.get(index)
            .map(|arg| arg.parse::<usize>().unwrap())
            .unwrap_or(default)
    };
    let count = number(2, 1024);
    let rounds = number(3, 1024);
    let capacity = number(4, 1024);
    assert!(count > 0 && rounds > 0);
    assert!(["readiness", "pending", "register", "register_reused"].contains(&mode));
    let registering = mode.starts_with("register");
    let mut fixtures: Vec<_> = (0..if registering { rounds } else { 1 })
        .map(|_| Fixture::new(count, capacity))
        .collect();
    if mode == "register_reused" {
        for fixture in &mut fixtures {
            let mut warm: Vec<_> = fixture.timer.with(|| {
                let deadline = Instant::now() + Duration::from_secs(1);
                (0..count)
                    .map(|_| Box::pin(sleep_until(deadline)))
                    .collect()
            });
            let mut cx = Context::from_waker(Waker::noop());
            for sleep in &mut warm {
                let _ = black_box(sleep.as_mut().poll(&mut cx));
            }
        }
    }
    if !registering {
        fixtures[0].poll();
        // Populate both cache lines and branch history outside the measured call.
        for _ in 0..16 {
            fixtures[0].poll();
            for sleep in fixtures[0].sleeps.iter().flatten() {
                black_box(black_box(sleep).is_elapsed());
            }
        }
    }
    let synchronize = std::env::var_os("SHEP_PROFILE_SYNC").is_some();
    handshake("ready", synchronize);
    let start = Instant::now();
    match mode {
        "readiness" => profile_readiness(&fixtures[0], rounds),
        "pending" => profile_pending(&mut fixtures[0], rounds),
        "register" | "register_reused" => profile_register(&mut fixtures),
        _ => unreachable!(),
    }
    let elapsed = start.elapsed();
    handshake("done", synchronize);
    println!(
        "mode={mode} count={count} rounds={rounds} capacity={capacity} ns_per_op={:.4}",
        elapsed.as_nanos() as f64 / (count as f64 * rounds as f64)
    );
    // Normal fixture destruction happens after counters have stopped.
}
