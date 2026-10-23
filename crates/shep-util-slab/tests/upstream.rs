// Adapted from slab 0.4.12 tests/slab.rs; see FORK.md and LICENSE-MIT.
// Copyright (c) 2019 Carl Lerche. Licensed under MIT.

#![forbid(unsafe_code)]

use shep_util_slab::Slab;

#[test]
fn insert_get_remove_one() {
    let mut slab = Slab::new();
    assert!(slab.is_empty());

    let key = slab.insert(10);

    assert_eq!(slab[key], 10);
    assert_eq!(slab.get(key), Some(&10));
    assert!(!slab.is_empty());
    assert!(slab.get(key).is_some());

    assert_eq!(slab.remove(key), 10);
    assert!(slab.get(key).is_none());
}

#[test]
fn insert_get_many() {
    let mut slab = Slab::with_capacity(10);

    for i in 0..10 {
        let key = slab.insert(i + 10);
        assert_eq!(slab[key], i + 10);
    }

    assert_eq!(slab.capacity(), 10);

    // Storing another one grows the slab
    let key = slab.insert(20);
    assert_eq!(slab[key], 20);

    // The public API guarantees enough capacity, not a particular growth factor.
    assert!(slab.capacity() >= 11);
}

#[test]
fn insert_get_remove_many() {
    let mut slab = Slab::with_capacity(10);
    let mut keys = vec![];

    for i in 0..10 {
        for j in 0..10 {
            let val = (i * 10) + j;

            let key = slab.insert(val);
            keys.push((key, val));
            assert_eq!(slab[key], val);
        }

        for (key, val) in keys.drain(..) {
            assert_eq!(val, slab.remove(key));
        }
    }

    assert_eq!(10, slab.capacity());
}

#[test]
#[should_panic(expected = "invalid key")]
fn invalid_get_panics() {
    let slab = Slab::<usize>::with_capacity(1);
    let _ = &slab[0];
}

#[test]
#[should_panic(expected = "invalid key")]
fn invalid_get_mut_panics() {
    let mut slab = Slab::<usize>::new();
    let _ = &mut slab[0];
}

#[test]
#[should_panic(expected = "invalid key")]
fn double_remove_panics() {
    let mut slab = Slab::<usize>::with_capacity(1);
    let key = slab.insert(123);
    slab.remove(key);
    slab.remove(key);
}

#[test]
#[should_panic(expected = "invalid key")]
fn invalid_remove_panics() {
    let mut slab = Slab::<usize>::with_capacity(1);
    slab.remove(0);
}

#[test]
fn slab_get_mut() {
    let mut slab = Slab::new();
    let key = slab.insert(1);

    slab[key] = 2;
    assert_eq!(slab[key], 2);

    *slab.get_mut(key).unwrap() = 3;
    assert_eq!(slab[key], 3);
}

#[test]
fn const_new() {
    static _SLAB: Slab<()> = Slab::new();
}
