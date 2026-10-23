//! Mixed operations checked against both an independent map and upstream slab.
#![forbid(unsafe_code)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

use hegel::TestCase;
use hegel::generators as gs;
use shep_util_slab::Slab;

#[derive(Default)]
struct Counts {
    created: Cell<usize>,
    dropped: Cell<usize>,
}

struct Value {
    data: i64,
    counts: Rc<Counts>,
}

impl Value {
    fn new(data: i64, counts: Rc<Counts>) -> Self {
        counts.created.set(counts.created.get() + 1);
        Self { data, counts }
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        self.counts.dropped.set(self.counts.dropped.get() + 1);
    }
}

struct Machine {
    actual: Slab<Value>,
    upstream: slab::Slab<i64>,
    model: BTreeMap<usize, i64>,
    // Removed values still belong to the caller, independent of slot reuse.
    held: Vec<Value>,
    counts: Rc<Counts>,
    high_water: usize,
}

impl Machine {
    fn new(capacity: usize, counts: Rc<Counts>) -> Self {
        Self {
            actual: Slab::with_capacity(capacity),
            upstream: slab::Slab::with_capacity(capacity),
            model: BTreeMap::new(),
            held: Vec::new(),
            counts,
            high_water: 0,
        }
    }

    #[hegel::test_helper]
    fn key(&self, tc: &TestCase) -> usize {
        let pick = tc.draw(gs::integers::<usize>().max_value(self.model.len() + 2));
        if pick < self.model.len() {
            *self.model.keys().nth(pick).unwrap()
        } else if pick == self.model.len() {
            self.actual.vacant_key()
        } else if pick == self.model.len() + 1 {
            self.high_water + 1
        } else {
            usize::MAX
        }
    }
}

#[hegel::state_machine]
impl Machine {
    #[rule(weight = 4)]
    fn insert(&mut self, tc: TestCase) {
        let data = tc.draw(gs::integers::<i64>());
        let predicted = self.actual.vacant_key();
        assert!(!self.model.contains_key(&predicted));
        assert_eq!(predicted, self.upstream.vacant_key());
        let key = self.actual.insert(Value::new(data, self.counts.clone()));
        assert_eq!(key, predicted);
        assert_eq!(key, self.upstream.insert(data));
        assert_eq!(self.model.insert(key, data), None);
        self.high_water = self.high_water.max(key + 1);
    }

    #[rule(weight = 3)]
    fn remove(&mut self, tc: TestCase) {
        tc.assume(!self.model.is_empty());
        let pick = tc.draw(gs::integers::<usize>().max_value(self.model.len() - 1));
        let key = *self.model.keys().nth(pick).unwrap();
        let before = self.counts.dropped.get();
        let value = self.actual.remove(key);
        assert_eq!(self.counts.dropped.get(), before);
        assert_eq!(Some(value.data), self.model.remove(&key));
        assert_eq!(value.data, self.upstream.remove(key));
        if tc.draw(gs::weighted_booleans(0.5)) {
            self.held.push(value);
        } else {
            drop(value);
        }
    }

    #[rule(weight = 3)]
    fn lookup(&mut self, tc: TestCase) {
        let key = self.key(&tc);
        let expected = self.model.get(&key).copied();
        assert_eq!(self.actual.get(key).map(|value| value.data), expected);
        assert_eq!(self.upstream.get(key).copied(), expected);
    }

    #[rule(weight = 3)]
    fn mutate(&mut self, tc: TestCase) {
        let key = self.key(&tc);
        let data = tc.draw(gs::integers::<i64>());
        if self.model.contains_key(&key) && tc.draw(gs::weighted_booleans(0.5)) {
            self.actual[key].data = data;
            self.upstream[key] = data;
            *self.model.get_mut(&key).unwrap() = data;
        } else {
            let actual = self.actual.get_mut(key);
            let expected = self.model.get_mut(&key);
            let upstream = self.upstream.get_mut(key);
            assert_eq!(actual.is_some(), expected.is_some());
            assert_eq!(actual.is_some(), upstream.is_some());
            if let (Some(actual), Some(expected), Some(upstream)) = (actual, expected, upstream) {
                actual.data = data;
                *expected = data;
                *upstream = data;
            }
        }
    }

    #[rule]
    fn release_removed_value(&mut self, tc: TestCase) {
        tc.assume(!self.held.is_empty());
        let pick = tc.draw(gs::integers::<usize>().max_value(self.held.len() - 1));
        drop(self.held.swap_remove(pick));
    }

    #[rule]
    fn take_and_drop_slab(&mut self, _: TestCase) {
        drop(std::mem::take(&mut self.actual));
        drop(std::mem::take(&mut self.upstream));
        self.model.clear();
    }

    #[invariant(always_run)]
    fn agrees_with_model(&self, _: TestCase) {
        assert_eq!(self.actual.len(), self.model.len());
        assert_eq!(self.actual.len(), self.upstream.len());
        assert_eq!(self.actual.is_empty(), self.model.is_empty());
        assert!(self.actual.capacity() >= self.actual.len());
        assert_eq!(self.actual.vacant_key(), self.upstream.vacant_key());
        assert!(self.actual.get(self.actual.vacant_key()).is_none());
        for key in 0..self.high_water + 2 {
            let expected = self.model.get(&key).copied();
            assert_eq!(self.actual.get(key).map(|value| value.data), expected);
            assert_eq!(self.upstream.get(key).copied(), expected);
        }
        assert!(self.actual.get(usize::MAX).is_none());
        assert_eq!(
            self.counts.created.get(),
            self.counts.dropped.get() + self.model.len() + self.held.len(),
            "each value belongs to the slab, the caller, or has been dropped"
        );
    }
}

#[hegel::test(test_cases = 1000)]
fn operations_and_ownership_match_the_model(tc: TestCase) {
    let capacity = tc.draw(gs::integers::<usize>().max_value(64));
    let counts = Rc::new(Counts::default());
    hegel::stateful::machine(Machine::new(capacity, counts.clone()))
        .steps(150)
        .run(tc);
    assert_eq!(counts.created.get(), counts.dropped.get());
}

#[hegel::test(test_cases = 1000)]
fn preallocation_survives_random_holes_and_refills(tc: TestCase) {
    let requested = tc.draw(gs::integers::<usize>().max_value(128));
    let mut slab = Slab::with_capacity(requested);
    let capacity = slab.capacity();
    assert!(capacity >= requested);
    let mut keys = Vec::new();
    for data in 0..capacity {
        keys.push(slab.insert(data));
        assert_eq!(slab.capacity(), capacity);
    }
    let holes = tc.draw(gs::integers::<usize>().max_value(capacity));
    for _ in 0..holes {
        let pick = tc.draw(gs::integers::<usize>().max_value(keys.len() - 1));
        let key = keys.swap_remove(pick);
        assert_eq!(slab.remove(key), key);
    }
    for _ in 0..holes {
        let key = slab.vacant_key();
        assert!(slab.get(key).is_none());
        assert_eq!(slab.insert(key), key);
        assert_eq!(slab.capacity(), capacity);
    }
    assert_eq!(slab.len(), capacity);
    for key in 0..capacity {
        assert_eq!(slab.get(key), Some(&key));
    }
    slab.insert(42);
    assert!(slab.capacity() > capacity);
    assert_eq!(slab.len(), capacity + 1);
}
