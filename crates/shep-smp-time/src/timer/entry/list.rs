// Extracted from Monoio and adapted for shep-smp-time; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

//! Doubly linked lists of IDs into the timer-owned slab. All entry and neighbor
//! access is checked; sequential mutations avoid aliased mutable references.

use shep_util_slab::Slab;

use super::{Entry, EntryId};

#[derive(Debug, Default)]
pub(in crate::timer) struct EntryList {
    head: Option<EntryId>,
    tail: Option<EntryId>,
}

impl EntryList {
    pub(in crate::timer) const fn new() -> Self {
        Self {
            head: None,
            tail: None,
        }
    }

    pub(in crate::timer) fn push_front(&mut self, entries: &mut Slab<Entry>, key: EntryId) {
        assert_ne!(self.head, Some(key));
        debug_assert!(entries[key.index()].prev.is_none());
        debug_assert!(entries[key.index()].next.is_none());
        entries[key.index()].next = self.head;
        if let Some(head) = self.head {
            entries[head.index()].prev = Some(key);
        }
        self.head = Some(key);
        if self.tail.is_none() {
            self.tail = Some(key);
        }
    }

    pub(in crate::timer) fn pop_back(&mut self, entries: &mut Slab<Entry>) -> Option<EntryId> {
        let key = self.tail?;
        self.tail = entries[key.index()].prev;
        if let Some(prev) = self.tail {
            entries[prev.index()].next = None;
        } else {
            self.head = None;
        }
        entries[key.index()].prev = None;
        entries[key.index()].next = None;
        Some(key)
    }

    pub(in crate::timer) fn is_empty(&self) -> bool {
        if self.head.is_some() {
            return false;
        }
        assert!(self.tail.is_none());
        true
    }

    pub(in crate::timer) fn remove(
        &mut self,
        entries: &mut Slab<Entry>,
        key: EntryId,
    ) -> Option<EntryId> {
        let prev = entries[key.index()].prev;
        let next = entries[key.index()].next;
        if let Some(prev) = prev {
            debug_assert_eq!(entries[prev.index()].next, Some(key));
            entries[prev.index()].next = next;
        } else {
            if self.head != Some(key) {
                return None;
            }
            self.head = next;
        }
        if let Some(next) = next {
            debug_assert_eq!(entries[next.index()].prev, Some(key));
            entries[next.index()].prev = prev;
        } else {
            if self.tail != Some(key) {
                return None;
            }
            self.tail = prev;
        }
        entries[key.index()].prev = None;
        entries[key.index()].next = None;
        Some(key)
    }
}

// Adapt the four upstream list cases to owned entries and key identity.
#[cfg(test)]
mod tests {
    use super::*;

    fn entry(entries: &mut Slab<Entry>) -> EntryId {
        EntryId::new(entries.insert(Entry::new()))
    }

    fn collect(list: &mut EntryList, entries: &mut Slab<Entry>) -> Vec<EntryId> {
        let mut result = Vec::new();
        while let Some(key) = list.pop_back(entries) {
            result.push(key);
        }
        result
    }

    #[test]
    fn const_new() {
        const _: EntryList = EntryList::new();
    }

    #[test]
    fn push_and_drain() {
        let mut entries = Slab::new();
        let keys = [
            entry(&mut entries),
            entry(&mut entries),
            entry(&mut entries),
        ];
        let mut list = EntryList::new();
        assert!(list.is_empty());
        for key in keys {
            list.push_front(&mut entries, key);
        }
        assert!(!list.is_empty());
        assert_eq!(collect(&mut list, &mut entries), keys);
        assert!(list.is_empty());
        for key in keys {
            assert_eq!(entries[key.index()].prev, None);
            assert_eq!(entries[key.index()].next, None);
        }
    }

    #[test]
    fn push_pop_push_pop() {
        let mut entries = Slab::new();
        let a = entry(&mut entries);
        let b = entry(&mut entries);
        let mut list = EntryList::new();
        for key in [a, b] {
            list.push_front(&mut entries, key);
            assert_eq!(list.pop_back(&mut entries), Some(key));
            assert!(list.is_empty());
            assert_eq!(entries[key.index()].prev, None);
            assert_eq!(entries[key.index()].next, None);
        }
        assert_eq!(list.pop_back(&mut entries), None);
    }

    #[test]
    fn remove_by_id() {
        for count in 1..=3 {
            // Head, middle, tail, repeat removals, and missing detached node.
            for removals in [&[0, 1, 2][..], &[2, 1, 0], &[1, 1], &[0, 0], &[2, 2], &[3]] {
                let mut entries = Slab::new();
                let keys: Vec<_> = (0..4).map(|_| entry(&mut entries)).collect();
                let mut expected = keys[..count].to_vec();
                let mut list = EntryList::new();
                for &key in &expected {
                    list.push_front(&mut entries, key);
                }
                for &index in removals {
                    let key = keys[index];
                    let present = expected.contains(&key);
                    assert_eq!(list.remove(&mut entries, key).is_some(), present);
                    expected.retain(|&other| other != key);
                    assert_eq!(entries[key.index()].prev, None);
                    assert_eq!(entries[key.index()].next, None);
                    assert_eq!(list.head, expected.last().copied());
                    assert_eq!(list.tail, expected.first().copied());
                    for (position, &key) in expected.iter().enumerate() {
                        assert_eq!(
                            entries[key.index()].prev,
                            expected.get(position + 1).copied()
                        );
                        assert_eq!(
                            entries[key.index()].next,
                            position.checked_sub(1).map(|i| expected[i])
                        );
                    }
                }
                assert_eq!(collect(&mut list, &mut entries), expected);
                assert!(list.is_empty());
            }
        }
    }
}
