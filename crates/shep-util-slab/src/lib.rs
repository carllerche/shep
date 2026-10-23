// Adapted from slab 0.4.12's safe storage core; see FORK.md and LICENSE-MIT.
// Copyright (c) 2019 Carl Lerche. Licensed under MIT.

//! Reusable integer-keyed storage with checked access and no unsafe code.
//!
//! A removed key may be reused by the next insertion. Keys are not generations
//! and values may move when the backing allocation grows. Callers must unlink
//! a key before removing its value and must not keep references across growth.
//! `with_capacity` reserves backing storage; it does not insert values or set
//! a limit. There are no normal dependencies.
//!
//! ```
//! use shep_util_slab::Slab;
//!
//! let mut entries = Slab::with_capacity(1024);
//! let next = entries.vacant_key();
//! let key = entries.insert(42);
//! assert_eq!(key, next);
//! *entries.get_mut(key).unwrap() += 1;
//! assert_eq!(entries.remove(key), 43);
//! assert!(entries.is_empty());
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::mem;
use std::ops::{Index, IndexMut};

/// A growable collection whose occupied slots have reusable integer keys.
///
/// Insertion, lookup, and removal take constant time, except when insertion
/// grows the backing Vec. Vacant slots link to the next vacancy, so reusing a
/// slot does not allocate. Removing a value returns it without dropping it.
#[derive(Debug)]
pub struct Slab<T> {
    entries: Vec<Entry<T>>,
    len: usize,
    next: usize,
}

#[derive(Debug)]
enum Entry<T> {
    Vacant(usize),
    Occupied(T),
}

impl<T> Slab<T> {
    /// Create an empty slab without allocating.
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            len: 0,
            next: 0,
        }
    }

    /// Reserve space for at least `capacity` values without occupying slots.
    /// Insertion beyond this reservation grows the slab automatically.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            len: 0,
            next: 0,
        }
    }

    /// The number of values currently stored.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the slab contains no values.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The number of live values the slab can hold without reallocating.
    pub fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    /// The key the next insertion will return. Does not reserve or occupy it.
    pub fn vacant_key(&self) -> usize {
        self.next
    }

    /// Insert a value, reusing the most recently removed slot if available.
    /// Existing keys remain valid if the backing allocation grows.
    pub fn insert(&mut self, value: T) -> usize {
        let key = self.next;
        if key == self.entries.len() {
            self.entries.push(Entry::Occupied(value));
            self.next = key + 1;
        } else {
            let Entry::Vacant(next) = self.entries[key] else {
                unreachable!("free list points to an occupied slot");
            };
            self.entries[key] = Entry::Occupied(value);
            self.next = next;
        }
        self.len += 1;
        key
    }

    /// Get a value, or None if the key is vacant or out of bounds.
    pub fn get(&self, key: usize) -> Option<&T> {
        match self.entries.get(key) {
            Some(Entry::Occupied(value)) => Some(value),
            _ => None,
        }
    }

    /// Get a mutable value, or None if the key is vacant or out of bounds.
    pub fn get_mut(&mut self, key: usize) -> Option<&mut T> {
        match self.entries.get_mut(key) {
            Some(Entry::Occupied(value)) => Some(value),
            _ => None,
        }
    }

    /// Remove and return a value, making its key available for reuse.
    /// The returned value is not dropped by the slab.
    ///
    /// # Panics
    /// Panics with "invalid key" if the key is vacant or out of bounds.
    #[track_caller]
    pub fn remove(&mut self, key: usize) -> T {
        let entry = self.entries.get_mut(key).expect("invalid key");
        assert!(matches!(entry, Entry::Occupied(_)), "invalid key");
        let Entry::Occupied(value) = mem::replace(entry, Entry::Vacant(self.next)) else {
            unreachable!("occupied slot was checked above");
        };
        self.len -= 1;
        self.next = key;
        value
    }
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Index<usize> for Slab<T> {
    type Output = T;

    #[track_caller]
    fn index(&self, key: usize) -> &T {
        self.get(key).expect("invalid key")
    }
}

impl<T> IndexMut<usize> for Slab<T> {
    #[track_caller]
    fn index_mut(&mut self, key: usize) -> &mut T {
        self.get_mut(key).expect("invalid key")
    }
}
