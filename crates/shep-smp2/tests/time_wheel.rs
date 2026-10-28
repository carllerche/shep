use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use shep_smp2::time::{Duration, Instant, Sleep, Timer, sleep_until};

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

impl WakeCount {
    fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

struct Entry {
    sleep: Pin<Box<Sleep>>,
    wakes: Arc<WakeCount>,
    // The reference model knows only absolute deadlines, never wheel buckets.
    deadline: u64,
}

impl Entry {
    fn new(timer: &Timer, epoch: Instant, deadline: u64) -> Self {
        Self {
            sleep: Box::pin(timer.with(|| sleep_until(at(epoch, deadline)))),
            wakes: Arc::default(),
            deadline,
        }
    }

    fn poll(&mut self) -> Poll<()> {
        let waker = Waker::from(self.wakes.clone());
        self.sleep.as_mut().poll(&mut Context::from_waker(&waker))
    }

    fn reset(&mut self, epoch: Instant, deadline: u64) {
        self.deadline = deadline;
        self.sleep.as_mut().reset(at(epoch, deadline));
    }
}

fn at(epoch: Instant, tick: u64) -> Instant {
    epoch + Duration::from_millis(tick)
}

#[test]
fn deadlines_on_both_sides_of_every_level_boundary() {
    // Also cross the top wheel's ~2-year range and start after a wrap.
    let epoch = Instant::now();
    for boundary in (1..=6).map(|level| 64_u64.pow(level)) {
        for elapsed in [0, boundary - 2, boundary + 7] {
            let timer = Timer::new(epoch);
            timer.advance(at(epoch, elapsed));
            let mut entries: Vec<_> = [boundary - 1, boundary, boundary + 1]
                .map(|offset| Entry::new(&timer, epoch, elapsed + offset))
                .into();
            for entry in &mut entries {
                assert!(entry.poll().is_pending());
            }
            for index in 0..entries.len() {
                let tick = entries[index].deadline;
                timer.advance(at(epoch, tick - 1));
                assert!(entries[index].poll().is_pending());
                timer.advance(at(epoch, tick));
                for (other_index, entry) in entries.iter_mut().enumerate() {
                    assert_eq!(
                        entry.poll().is_ready(),
                        other_index <= index,
                        "boundary={boundary}, elapsed={elapsed}, tick={tick}, index={index}, other={other_index}"
                    );
                    assert_eq!(entry.wakes.get(), usize::from(other_index <= index));
                }
            }
            assert_eq!(timer.next_wake(), None);
        }
    }
}

#[test]
fn deadlines_beyond_the_top_wheel_range_survive_repeated_wraps() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let cycle = 1_u64 << 36;
    // Both entries occupy the same top-level bucket over multiple rotations.
    let mut entries = [
        Entry::new(&timer, epoch, 3 * cycle + 123),
        Entry::new(&timer, epoch, 4 * cycle + 123),
    ];
    for entry in &mut entries {
        assert!(entry.poll().is_pending());
    }
    let mut elapsed = 0;
    for _ in 0..64 {
        let Some(next) = timer.next_wake() else {
            break;
        };
        assert!(next > at(epoch, elapsed), "wheel must make progress");
        elapsed = (next - epoch).as_millis().try_into().unwrap();
        timer.advance(next);
        for entry in &mut entries {
            assert_eq!(entry.poll().is_ready(), elapsed >= entry.deadline);
            assert_eq!(entry.wakes.get(), usize::from(elapsed >= entry.deadline));
        }
    }
    assert_eq!(timer.next_wake(), None);
    assert!(entries.iter().all(|entry| entry.sleep.is_elapsed()));
}

#[test]
fn cancelling_head_middle_and_tail_leaves_shared_bucket_siblings_intact() {
    for deadline in [1, 100, 4097, 262_145, 16_777_217, 1_073_741_825] {
        let epoch = Instant::now();
        let timer = Timer::new(epoch);
        let mut entries: Vec<_> = (0..7)
            .map(|_| Some(Entry::new(&timer, epoch, deadline)))
            .collect();
        for entry in entries.iter_mut().flatten() {
            assert!(entry.poll().is_pending());
        }
        for index in [0, 3, 6] {
            let entry = entries[index].take().unwrap();
            let wakes = entry.wakes.clone();
            drop(entry);
            assert_eq!(wakes.get(), 0, "cancellation must not wake the task");
        }
        timer.advance(at(epoch, deadline));
        for entry in entries.iter_mut().flatten() {
            assert!(entry.poll().is_ready());
            assert_eq!(entry.wakes.get(), 1);
        }
        assert_eq!(timer.next_wake(), None);
    }
}

#[test]
fn cancelling_lazy_extensions_before_and_after_cascading_unlinks_cached_buckets() {
    for cascade in [false, true] {
        let epoch = Instant::now();
        let timer = Timer::new(epoch);
        let mut entry = Entry::new(&timer, epoch, 100);
        assert!(entry.poll().is_pending());
        entry.reset(epoch, 4097);
        entry.reset(epoch, 262_145);
        if cascade {
            timer.advance(at(epoch, 100));
            assert!(entry.poll().is_pending());
        }
        let wakes = entry.wakes.clone();
        drop(entry);
        assert_eq!(timer.next_wake(), None);
        timer.advance(at(epoch, 262_145));
        assert_eq!(wakes.get(), 0);
    }
}

#[test]
fn resets_before_first_poll_and_into_the_past_obey_current_elapsed_time() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    timer.advance(at(epoch, 100));
    let mut entry = Entry::new(&timer, epoch, 4096);
    entry.reset(epoch, 102);
    assert_eq!(entry.sleep.deadline(), at(epoch, 102));
    assert!(entry.poll().is_pending());
    entry.reset(epoch, 99);
    assert!(entry.sleep.is_elapsed());
    assert_eq!(
        entry.wakes.get(),
        1,
        "resetting a pending task must wake it"
    );
    assert!(entry.poll().is_ready());
    entry.reset(epoch, 101);
    assert!(!entry.sleep.is_elapsed());
    assert!(entry.poll().is_pending());
    timer.advance(at(epoch, 101));
    assert!(entry.poll().is_ready());
    assert_eq!(entry.wakes.get(), 2);
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn mixed_operations_match_an_absolute_deadline_model() {
    // A reproducible model test without a random-number dependency. Each seed
    // exercises 256 registrations, resets, drops, and time jumps, polling all
    // surviving entries and checking exact wake counts after each advance.
    const OFFSETS: [u64; 18] = [
        1,
        2,
        63,
        64,
        65,
        4095,
        4096,
        4097,
        262_143,
        262_144,
        262_145,
        16_777_215,
        16_777_216,
        1_073_741_823,
        1_073_741_824,
        (1 << 36) - 1,
        1 << 36,
        (1 << 36) + 1,
    ];
    for seed in [1_u64, 42, 0xdead_beef, 0xcafe_f00d] {
        let epoch = Instant::now();
        let timer = Timer::new(epoch);
        let mut entries: Vec<Option<Entry>> = (0..16).map(|_| None).collect();
        let mut random = seed;
        let mut elapsed = 0_u64;
        for step in 0..256 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let index = ((random >> 32) as usize) % entries.len();
            let offset = OFFSETS[((random >> 16) as usize) % OFFSETS.len()];
            let deadline = if step % 11 == 0 {
                elapsed.saturating_sub(offset)
            } else {
                elapsed + offset
            };
            match step % 5 {
                0 | 1 => entries[index] = Some(Entry::new(&timer, epoch, deadline)),
                2 => {
                    if let Some(entry) = &mut entries[index] {
                        entry.reset(epoch, deadline);
                    }
                }
                3 => {
                    if let Some(entry) = entries[index].take() {
                        let wakes = entry.wakes.clone();
                        let before = wakes.get();
                        drop(entry);
                        assert_eq!(wakes.get(), before);
                    }
                }
                4 => {
                    let expected: Vec<_> = entries
                        .iter()
                        .map(|entry| {
                            entry.as_ref().map(|entry| {
                                entry.wakes.get()
                                    + usize::from(
                                        entry.deadline > elapsed
                                            && entry.deadline <= elapsed + offset,
                                    )
                            })
                        })
                        .collect();
                    elapsed += offset;
                    timer.advance(at(epoch, elapsed));
                    for (entry, expected) in entries.iter().zip(expected) {
                        if let Some(entry) = entry {
                            assert_eq!(
                                Some(entry.wakes.get()),
                                expected,
                                "seed={seed}, step={step}"
                            );
                        }
                    }
                }
                _ => unreachable!(),
            }
            for entry in entries.iter_mut().flatten() {
                let ready = entry.deadline <= elapsed;
                assert_eq!(entry.poll().is_ready(), ready, "seed={seed}, step={step}");
                assert_eq!(entry.sleep.is_elapsed(), ready);
            }
            match timer.next_wake() {
                Some(next) => {
                    let earliest = entries
                        .iter()
                        .flatten()
                        .filter(|entry| entry.deadline > elapsed)
                        .map(|entry| entry.deadline)
                        .min()
                        .unwrap();
                    assert!(next > at(epoch, elapsed));
                    assert!(next <= at(epoch, earliest));
                }
                None => assert!(
                    entries
                        .iter()
                        .flatten()
                        .all(|entry| entry.deadline <= elapsed)
                ),
            }
        }
        drop(entries);
        assert_eq!(timer.next_wake(), None);
    }
}
