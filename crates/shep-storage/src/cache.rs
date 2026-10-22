//! Recently appended records, so followers and the state machine catching up
//! on the tail read from memory instead of disk. A simplified Redpanda batch
//! cache: one contiguous window per log ending at the last append, bounded in
//! bytes, evicting the oldest first.

use std::collections::VecDeque;

use crate::record::Record;

pub(crate) struct Cache {
    /// Records `first..first + records.len()`.
    records: VecDeque<Record>,
    first: u64,
    bytes: usize,
    capacity: usize,
}

impl Cache {
    pub(crate) fn new(capacity: usize) -> Cache {
        Cache {
            records: VecDeque::new(),
            first: 0,
            bytes: 0,
            capacity,
        }
    }

    pub(crate) fn get(&self, index: u64) -> Option<&Record> {
        self.records.get(index.checked_sub(self.first)? as usize)
    }

    /// Adds the record after the last one; any other position restarts the
    /// window.
    pub(crate) fn push(&mut self, record: Record) {
        if self.capacity == 0 {
            return;
        }
        if self.first + self.records.len() as u64 != record.index {
            self.clear();
            self.first = record.index;
        }
        self.bytes += record.disk_len();
        self.records.push_back(record);
        while self.bytes > self.capacity {
            let r = self.records.pop_front().unwrap();
            self.bytes -= r.disk_len();
            self.first += 1;
        }
    }

    /// Drops records `index` and above.
    pub(crate) fn truncate(&mut self, index: u64) {
        while self.first + (self.records.len() as u64) > index {
            let Some(r) = self.records.pop_back() else {
                break;
            };
            self.bytes -= r.disk_len();
        }
    }

    /// Drops records below `index`.
    pub(crate) fn evict_below(&mut self, index: u64) {
        while self.first < index {
            let Some(r) = self.records.pop_front() else {
                self.first = index;
                break;
            };
            self.bytes -= r.disk_len();
            self.first += 1;
        }
    }

    fn clear(&mut self) {
        self.records.clear();
        self.bytes = 0;
    }
}
