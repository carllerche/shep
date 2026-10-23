#![forbid(unsafe_code)]

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use shep_util_slab::Slab;

struct Dropped(Rc<Cell<usize>>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn removal_returns_ownership_and_drops_each_local_value_exactly_once() {
    let drops = Rc::new(Cell::new(0));
    let mut slab = Slab::with_capacity(4);
    let first = slab.insert(Dropped(drops.clone()));
    slab.insert(Dropped(drops.clone()));
    let removed = slab.remove(first);
    assert_eq!(
        drops.get(),
        0,
        "remove must return ownership without dropping"
    );
    assert_eq!(slab.len(), 1);
    assert_eq!(slab.insert(Dropped(drops.clone())), first);
    drop(slab);
    assert_eq!(drops.get(), 2, "only occupied slots are dropped");
    drop(removed);
    assert_eq!(drops.get(), 3);
}

#[test]
fn vacant_and_out_of_bounds_keys_leave_the_free_list_intact() {
    let mut slab = Slab::with_capacity(4);
    let a = slab.insert(10);
    let b = slab.insert(20);
    slab.remove(a);
    for key in [a, 2, 100, usize::MAX] {
        assert_eq!(slab.get(key), None);
        assert_eq!(slab.get_mut(key), None);
        assert!(catch_unwind(AssertUnwindSafe(|| slab.remove(key))).is_err());
        assert_eq!(slab.len(), 1);
        assert_eq!(slab[b], 20);
        assert_eq!(slab.vacant_key(), a);
    }
    assert_eq!(slab.insert(30), a);
    assert_eq!(slab[a], 30);
    assert_eq!(slab.len(), 2);
}

#[test]
fn invalid_mutable_indexing_cannot_change_another_value() {
    let mut slab = Slab::new();
    let key = slab.insert(42);
    let vacant = slab.insert(7);
    slab.remove(vacant);
    for invalid in [vacant, usize::MAX] {
        assert!(catch_unwind(AssertUnwindSafe(|| slab[invalid] = 0)).is_err());
        assert_eq!(slab[key], 42);
        assert_eq!(slab.len(), 1);
    }
}

#[test]
fn zero_sized_values_have_distinct_keys_and_reuse_freed_slots() {
    let mut slab = Slab::with_capacity(2);
    let a = slab.insert(());
    let b = slab.insert(());
    assert_ne!(a, b);
    slab.remove(a);
    assert_eq!(slab.insert(()), a);
    assert_eq!(slab.len(), 2);
    assert_eq!(slab.get(b), Some(&()));
}

#[test]
fn predicted_keys_do_not_occupy_slots_and_reuse_is_lifo() {
    let mut slab = Slab::with_capacity(3);
    let capacity = slab.capacity();
    for expected in 0..3 {
        assert_eq!(slab.vacant_key(), expected);
        assert_eq!(slab.vacant_key(), expected);
        assert_eq!(slab.len(), expected);
        assert_eq!(slab.insert(expected), expected);
    }
    slab.remove(0);
    slab.remove(2);
    assert_eq!(slab.vacant_key(), 2);
    assert_eq!(slab.insert(30), 2);
    assert_eq!(slab.insert(10), 0);
    assert_eq!(slab.capacity(), capacity);
    assert_eq!(slab[1], 1);
}

#[test]
fn reservation_is_a_hint_and_growth_preserves_occupied_keys() {
    let mut slab = Slab::with_capacity(2);
    let capacity = slab.capacity();
    for i in 0..capacity {
        slab.insert(i);
        assert_eq!(slab.capacity(), capacity);
    }
    slab.remove(0);
    assert_eq!(slab.insert(100), 0);
    assert_eq!(slab.capacity(), capacity);
    for i in capacity..capacity + 100 {
        assert_eq!(slab.insert(i), i);
    }
    assert!(slab.capacity() > capacity);
    assert_eq!(slab[0], 100);
    for i in 1..capacity + 100 {
        assert_eq!(slab[i], i);
    }
}

#[test]
fn taking_the_slab_releases_its_values_and_leaves_a_reusable_default() {
    let drops = Rc::new(Cell::new(0));
    let mut slab = Slab::new();
    slab.insert(Dropped(drops.clone()));
    slab.insert(Dropped(drops.clone()));
    drop(std::mem::take(&mut slab));
    assert_eq!(drops.get(), 2);
    assert!(slab.is_empty());
    assert_eq!(slab.capacity(), 0);
    assert_eq!(slab.insert(Dropped(drops.clone())), 0);
    drop(slab);
    assert_eq!(drops.get(), 3);
}

#[test]
fn a_panicking_value_destructor_does_not_leak_other_values() {
    struct PanicsOnce {
        drops: Rc<Cell<usize>>,
        panic: bool,
    }
    impl Drop for PanicsOnce {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            assert!(!self.panic, "intentional payload drop panic");
        }
    }
    let drops = Rc::new(Cell::new(0));
    let mut slab = Slab::with_capacity(4);
    for panic in [false, true, false, false] {
        slab.insert(PanicsOnce {
            drops: drops.clone(),
            panic,
        });
    }
    drop(slab.remove(0));
    assert!(catch_unwind(AssertUnwindSafe(|| drop(slab))).is_err());
    assert_eq!(drops.get(), 4);
}
