//! The leader's validation table (§10.3): the write sets of a recent window of
//! entries, each tagged with its index once that's known.
//!
//! A proposal conflicts if any entry in the table that isn't in its read
//! version wrote a key in its read set or inside one of its read ranges. Write
//! sets enter the table when the leader hands the entry to Raft, before it
//! commits, so the next proposal is checked against it; until Raft reports its
//! index, an entry counts as unseen by everyone.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Bound;

use shep_wal_api::{Dot, Dvv, KeyRange, PartitionId};

pub struct Table {
    partition: PartitionId,
    /// The term the table was built for. It's only valid while this node
    /// leads in that term.
    term: u64,
    /// Every entry above this index is in the table.
    floor: u64,
    /// The sequence number of `slots[0]`.
    front_seq: u64,
    slots: VecDeque<Slot>,
    /// Single keys written, to the sequence numbers that wrote them, oldest
    /// first.
    points: BTreeMap<Vec<u8>, VecDeque<u64>>,
    /// Sequence numbers of entries that wrote ranges wider than one key.
    ranged: BTreeSet<u64>,
}

struct Slot {
    index: SlotIndex,
    commit_time_us: u64,
    points: Vec<Vec<u8>>,
    ranges: Vec<KeyRange>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotIndex {
    /// Handed to Raft; its index isn't known yet.
    Pending,
    Known(u64),
    /// Appended at this index, but void: it wrote nothing.
    Void(u64),
    /// Refused before it was appended: it wrote nothing and holds no index.
    NotAppended,
}

impl Table {
    /// An empty table whose entries all lie above `floor`.
    pub fn new(partition: PartitionId, term: u64, floor: u64) -> Table {
        Table {
            partition,
            term,
            floor,
            front_seq: 0,
            slots: VecDeque::new(),
            points: BTreeMap::new(),
            ranged: BTreeSet::new(),
        }
    }

    pub fn term(&self) -> u64 {
        self.term
    }

    pub fn floor(&self) -> u64 {
        self.floor
    }

    /// The number of entries in the table.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Adds an entry's write set, returning its sequence number.
    pub fn push(&mut self, index: SlotIndex, write_set: &[KeyRange], commit_time_us: u64) -> u64 {
        let seq = self.front_seq + self.slots.len() as u64;
        let mut slot = Slot {
            index,
            commit_time_us,
            points: Vec::new(),
            ranges: Vec::new(),
        };
        for w in write_set.iter().filter(|w| !w.is_empty()) {
            match w.as_point() {
                Some(k) => {
                    self.points.entry(k.to_vec()).or_default().push_back(seq);
                    slot.points.push(k.to_vec());
                }
                None => slot.ranges.push(w.clone()),
            }
        }
        if !slot.ranges.is_empty() {
            self.ranged.insert(seq);
        }
        self.slots.push_back(slot);
        seq
    }

    pub fn resolve(&mut self, seq: u64, index: SlotIndex) {
        if let Some(slot) = seq
            .checked_sub(self.front_seq)
            .and_then(|i| self.slots.get_mut(i as usize))
        {
            slot.index = index;
        }
    }

    fn slot(&self, seq: u64) -> &Slot {
        &self.slots[(seq - self.front_seq) as usize]
    }

    /// Whether the entry may have written something `read_version` didn't
    /// see.
    fn unseen(&self, seq: u64, read_version: &Dvv) -> bool {
        match self.slot(seq).index {
            SlotIndex::Pending => true,
            SlotIndex::Known(i) => !read_version.contains(Dot::new(self.partition, i)),
            SlotIndex::Void(_) | SlotIndex::NotAppended => false,
        }
    }

    /// Whether an entry in the table that `read_version` doesn't contain wrote
    /// a key in `read_set`.
    pub fn conflicts(&self, read_set: &[KeyRange], read_version: &Dvv) -> bool {
        read_set.iter().filter(|r| !r.is_empty()).any(|r| {
            let points = match r.as_point() {
                Some(k) => self
                    .points
                    .get(k)
                    .into_iter()
                    .flatten()
                    .any(|&s| self.unseen(s, read_version)),
                None => {
                    let end = r.end().map_or(Bound::Unbounded, Bound::Excluded);
                    self.points
                        .range::<[u8], _>((Bound::Included(r.start()), end))
                        .flat_map(|(_, seqs)| seqs)
                        .any(|&s| self.unseen(s, read_version))
                }
            };
            points
                || self.ranged.iter().any(|&s| {
                    self.slot(s).ranges.iter().any(|w| w.intersects(r))
                        && self.unseen(s, read_version)
                })
        })
    }

    /// Drops the oldest entries while they're older than `cutoff_us` or the
    /// table holds more than `max_slots`. An entry whose index isn't known yet
    /// stops it, since everything above the floor must stay.
    pub fn evict(&mut self, cutoff_us: u64, max_slots: usize) {
        while let Some(front) = self.slots.front() {
            let index = match front.index {
                SlotIndex::Known(i) | SlotIndex::Void(i) => Some(i),
                // Holds no index, so it moves no floor.
                SlotIndex::NotAppended => None,
                SlotIndex::Pending => break,
            };
            if front.commit_time_us >= cutoff_us && self.slots.len() <= max_slots {
                break;
            }
            let slot = self.slots.pop_front().unwrap();
            let seq = self.front_seq;
            self.front_seq += 1;
            if let Some(index) = index {
                self.floor = self.floor.max(index);
            }
            for k in slot.points {
                if let Some(seqs) = self.points.get_mut(&k) {
                    if seqs.front() == Some(&seq) {
                        seqs.pop_front();
                    }
                    if seqs.is_empty() {
                        self.points.remove(&k);
                    }
                }
            }
            self.ranged.remove(&seq);
        }
    }
}
