//! A length prefix claiming far more data than the input holds must fail
//! without reserving memory for it.
//!
//! This file is its own test binary with a single test, so the counting
//! allocator sees only this test's allocations.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use shep_codec::{Decode, DecodeError, from_bytes};

/// Records the largest single allocation.
struct Counting;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[derive(shep_codec::Encode, shep_codec::Decode, Debug)]
struct Holder {
    items: Vec<[u64; 16]>,
}

/// Decodes `input` as a `T`, expecting failure, and checks no allocation was
/// anywhere near what the prefix claimed.
fn check<T: Decode + std::fmt::Debug>(input: &[u8]) {
    let input = input.to_vec();
    LARGEST.store(0, Ordering::Relaxed);
    let result = from_bytes::<T>(&input);
    let largest = LARGEST.load(Ordering::Relaxed);
    assert!(
        matches!(result, Err(DecodeError::UnexpectedEnd { .. })),
        "{}: {result:?}",
        std::any::type_name::<T>()
    );
    assert!(
        largest <= 4096,
        "{}: allocated {largest} bytes",
        std::any::type_name::<T>()
    );
}

#[test]
fn hostile_length_prefixes() {
    let max = [0xff, 0xff, 0xff, 0xff];
    // A u32::MAX count followed by a few bytes of content (distinct, so
    // sets and maps don't stop early on a duplicate).
    let mut some = max.to_vec();
    some.extend(0..64u8);

    for input in [&max[..], &some[..]] {
        check::<String>(input);
        check::<Vec<u8>>(input);
        check::<Vec<u8>>(input);
        check::<Vec<u64>>(input);
        check::<Vec<[u64; 64]>>(input);
        check::<Vec<String>>(input);
        check::<Vec<Vec<u8>>>(input);
        check::<BTreeMap<u32, u32>>(input);
        check::<BTreeSet<u32>>(input);
        check::<HashMap<u64, [u8; 32]>>(input);
        check::<HashSet<u64>>(input);
        check::<Option<Vec<u64>>>(&[&[1][..], input].concat());
    }

    // Inside an envelope whose size is honest.
    let mut enveloped = vec![0, 0, 4 + 8, 0, 0, 0];
    enveloped.extend_from_slice(&max);
    enveloped.extend_from_slice(&[0; 8]);
    check::<Holder>(&enveloped);

    // An envelope size of u32::MAX.
    let mut lying = vec![0, 0, 0xff, 0xff, 0xff, 0xff];
    lying.extend_from_slice(&[0; 32]);
    check::<Holder>(&lying);
}
