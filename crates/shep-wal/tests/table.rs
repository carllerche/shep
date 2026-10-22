//! Random operation sequences against the validation table and a model of it:
//! a list of every entry ever pushed, scanned linearly.
//!
//! Two properties:
//!
//! - The table agrees with the model's live window on every conflict check
//!   and on the floor, so its point and range indexes find exactly what a
//!   scan finds.
//! - Eviction loses nothing: for a read version that reaches the floor (the
//!   leader refuses the rest as too old), the table agrees with a scan over
//!   the whole history, evicted entries included.
//!
//! Entries are admitted, resolved and evicted as the leader does it: indexes
//! are assigned in admission order, some admissions are never appended, some
//! are void, and some are never resolved.

use hegel::TestCase;
use hegel::generators as gs;
use shep_wal::table::{SlotIndex, Table};
use shep_wal_api::{Dot, Dvv, KeyRange, PartitionId};

const P: PartitionId = PartitionId(1);
const OTHER: PartitionId = PartitionId(2);

/// How an admitted entry ends up, revealed when it's resolved.
#[derive(Clone, Copy, Debug)]
enum Fate {
    Appended,
    Void,
    NotAppended,
    /// Never resolved; stays pending.
    Unknown,
}

struct Slot {
    index: SlotIndex,
    commit_time_us: u64,
    writes: Vec<KeyRange>,
}

impl Slot {
    fn unseen(&self, version: &Dvv) -> bool {
        match self.index {
            SlotIndex::Pending => true,
            SlotIndex::Known(i) => !version.contains(Dot::new(P, i)),
            SlotIndex::Void(_) | SlotIndex::NotAppended => false,
        }
    }

    fn conflicts(&self, reads: &[KeyRange], version: &Dvv) -> bool {
        self.unseen(version)
            && reads
                .iter()
                .any(|r| self.writes.iter().any(|w| w.intersects(r)))
    }
}

/// The table under test, beside the model.
struct Machine {
    table: Table,
    /// Every entry pushed, by sequence number.
    history: Vec<Slot>,
    /// The sequence number of the oldest entry still in the window.
    front: usize,
    floor: u64,
    /// The next log index Raft assigns.
    next_index: u64,
    now: u64,
    /// Admitted entries not resolved yet, with what resolving them reveals.
    pending: Vec<(u64, Option<SlotIndex>)>,
}

impl Machine {
    /// A new leader's table, with the entries above the floor replayed.
    fn new(floor: u64, replay: Vec<Vec<KeyRange>>) -> Machine {
        let mut m = Machine {
            table: Table::new(P, 1, floor),
            history: Vec::new(),
            front: 0,
            floor,
            next_index: floor + 1,
            now: 0,
            pending: Vec::new(),
        };
        for writes in replay {
            let index = SlotIndex::Known(m.next_index);
            m.next_index += 1;
            let seq = m.table.push(index, &writes, m.now);
            assert_eq!(seq, m.history.len() as u64);
            m.history.push(Slot {
                index,
                commit_time_us: m.now,
                writes,
            });
        }
        m
    }

    fn live(&self) -> &[Slot] {
        &self.history[self.front..]
    }

    fn model_evict(&mut self, cutoff_us: u64, max_slots: usize) {
        while let Some(front) = self.live().first() {
            let index = match front.index {
                SlotIndex::Known(i) | SlotIndex::Void(i) => Some(i),
                SlotIndex::NotAppended => None,
                SlotIndex::Pending => break,
            };
            if front.commit_time_us >= cutoff_us && self.live().len() <= max_slots {
                break;
            }
            self.front += 1;
            if let Some(i) = index {
                self.floor = self.floor.max(i);
            }
        }
    }

    /// A read version near the log's end, maybe with extra dots and a dot of
    /// another partition, which must not count.
    #[hegel::test_helper]
    fn version(&self, tc: &TestCase) -> Dvv {
        let below = |d: u64| self.next_index.saturating_sub(d);
        let behind = tc.draw(gs::integers::<u64>().max_value(7));
        let mut dvv = Dvv::new().with_prefix(P, below(behind));
        let extras = tc.draw(gs::vecs(gs::integers::<u64>().max_value(7)).max_size(2));
        for d in extras {
            dvv.insert(Dot::new(P, below(d)));
        }
        let other = tc.draw(gs::weighted_booleans(0.2));
        if other {
            let other_index = tc.draw(gs::integers::<u64>().max_value(7));
            dvv.insert(Dot::new(OTHER, other_index));
        }
        dvv
    }
}

#[hegel::state_machine]
impl Machine {
    #[rule(weight = 4)]
    fn admit(&mut self, tc: TestCase) {
        let writes = ranges(&tc);
        let fate = tc.draw(gs::sampled_from(vec![
            Fate::Appended,
            Fate::Appended,
            Fate::Appended,
            Fate::Void,
            Fate::NotAppended,
            Fate::Unknown,
        ]));
        self.now += tc.draw(gs::integers::<u64>().max_value(9));
        let seq = self.table.push(SlotIndex::Pending, &writes, self.now);
        assert_eq!(seq, self.history.len() as u64);
        // Raft appends in admission order, void entries included.
        let resolved = match fate {
            Fate::Appended | Fate::Void => {
                let i = self.next_index;
                self.next_index += 1;
                Some(match fate {
                    Fate::Appended => SlotIndex::Known(i),
                    _ => SlotIndex::Void(i),
                })
            }
            Fate::NotAppended => Some(SlotIndex::NotAppended),
            Fate::Unknown => None,
        };
        self.pending.push((seq, resolved));
        self.history.push(Slot {
            index: SlotIndex::Pending,
            commit_time_us: self.now,
            writes,
        });
    }

    #[rule(weight = 3)]
    fn resolve(&mut self, tc: TestCase) {
        tc.assume(!self.pending.is_empty());
        let pick = tc.draw(gs::integers::<usize>().max_value(self.pending.len() - 1));
        let (seq, resolved) = self.pending.remove(pick);
        if let Some(index) = resolved {
            self.table.resolve(seq, index);
            self.history[seq as usize].index = index;
        }
    }

    #[rule(weight = 4)]
    fn query(&mut self, tc: TestCase) {
        let reads = ranges(&tc);
        let version = self.version(&tc);
        let got = self.table.conflicts(&reads, &version);
        let live = self.live().iter().any(|s| s.conflicts(&reads, &version));
        assert_eq!(got, live, "window disagrees: {reads:?} at {version}");
        if version.prefix(P) >= self.floor {
            let all = self.history.iter().any(|s| s.conflicts(&reads, &version));
            assert_eq!(got, all, "evicted a conflict: {reads:?} at {version}");
        }
    }

    #[rule(weight = 2)]
    fn evict(&mut self, tc: TestCase) {
        let window = tc.draw(gs::integers::<u64>().max_value(29));
        let max_slots = tc.draw(gs::integers::<usize>().max_value(7));
        let cutoff = self.now.saturating_sub(window);
        let before = self.table.floor();
        self.table.evict(cutoff, max_slots);
        self.model_evict(cutoff, max_slots);
        assert!(self.table.floor() >= before, "floor went back");
    }

    #[invariant(always_run)]
    fn window_matches(&self, _: TestCase) {
        assert_eq!(self.table.floor(), self.floor);
        assert_eq!(self.table.len(), self.live().len());
    }
}

/// A key from a tiny alphabet, so ranges and points collide often. `0x00`
/// and `0xff` are the edges of points and prefixes.
fn key(tc: &TestCase) -> Vec<u8> {
    tc.draw_named(
        "key",
        gs::vecs(gs::sampled_from(vec![0x00u8, 0x01, 0xff])).max_size(2),
    )
}

#[hegel::test_helper]
fn range(tc: &TestCase) -> KeyRange {
    let kind = tc.draw(gs::integers::<u8>().max_value(7));
    match kind {
        0..=2 => KeyRange::point(key(tc)),
        3..=4 => KeyRange::new(key(tc), key(tc)),
        // A point spelled out as a range.
        5 => {
            let k = key(tc);
            let mut end = k.clone();
            end.push(0);
            KeyRange::new(k, end)
        }
        6 => KeyRange::starting_at(key(tc)),
        _ => KeyRange::prefix(key(tc)),
    }
}

#[hegel::test_helper]
fn ranges(tc: &TestCase) -> Vec<KeyRange> {
    let n = tc.draw(gs::integers::<usize>().max_value(3));
    (0..n).map(|_| range(tc)).collect()
}

#[hegel::test]
fn table_matches_model(tc: TestCase) {
    let floor = tc.draw(gs::integers::<u64>().max_value(3));
    let replayed = tc.draw(gs::integers::<usize>().max_value(5));
    let replay = (0..replayed).map(|_| ranges(&tc)).collect();
    hegel::stateful::machine(Machine::new(floor, replay))
        .steps(150)
        .run(tc);
}
